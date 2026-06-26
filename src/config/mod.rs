// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Configuration handling for language servers and session settings.

mod language;
mod linter;
pub(crate) mod merge;
mod parse;
mod server;
pub(crate) mod validate;

mod commands;

use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;

use crate::companions::CompanionRules;
use crate::logging::reaper::ReapPolicy;

pub use commands::{BuildContext, BuildGuidance, CommandsConfig, GuidanceEntry, ResolvedCommands};
pub use language::{DispatchMethod, LanguageConfig, ServerBinding};
pub use linter::LinterConfig;
pub use parse::{
    DEFAULT_SERVERS, ProjectConfig, SERVER_DEF_KEYS, config_sources, load_project_config,
};
pub use server::{DiagnosticPrecedence, ServerDef};

/// Notification delivery configuration.
///
/// Controls which tracing events are promoted to user-facing notifications
/// via `systemMessage`. Events below the threshold are silently dropped by
/// the notification queue sink.
///
/// # Examples
///
/// ```toml
/// [notifications]
/// threshold = "warn"
/// desktop = true
/// ```
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct NotificationConfig {
    /// Minimum severity for notification delivery.
    pub threshold: SeverityConfig,
    /// Whether OS-level desktop notifications are enabled for error events.
    /// Defaults to `true`. Set to `false` to suppress desktop notifications
    /// while keeping `systemMessage` delivery. The `CATENARY_NOTIFY=0`
    /// environment variable overrides this to `false`.
    pub desktop: Option<bool>,
}

/// Severity level for notification threshold configuration.
///
/// Deserialized from lowercase TOML strings (`"debug"`, `"info"`, `"warn"`,
/// `"error"`). Defaults to `Warn`.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SeverityConfig {
    /// Include debug-level events (most verbose).
    Debug,
    /// Include info-level and above.
    Info,
    /// Include warn-level and above (default).
    #[default]
    Warn,
    /// Only error-level events.
    Error,
}

impl From<SeverityConfig> for crate::logging::Severity {
    fn from(sc: SeverityConfig) -> Self {
        match sc {
            SeverityConfig::Debug => Self::Debug,
            SeverityConfig::Info => Self::Info,
            SeverityConfig::Warn => Self::Warn,
            SeverityConfig::Error => Self::Error,
        }
    }
}

/// Workspace-root configuration (`[roots]`).
///
/// User-level only. The lone field today is [`companions`](Self::companions);
/// the section exists so companion-root rules have a stable home as more
/// root-policy knobs land.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct RootsConfig {
    /// Companion-root derivation rules (`[roots.companions]`).
    ///
    /// A matcher → template map (see [`CompanionRules`]). **Absent ⇒ feature
    /// off** — Catenary ships no table and assumes no naming convention. Parsed
    /// only from the user config; a project `.catenary.toml` carrying `[roots]`
    /// is warned-and-ignored (it is not a project-allowed key) so a public repo
    /// cannot leak a private sibling path.
    pub companions: Option<CompanionRules>,
}

/// Overall configuration for Catenary.
///
/// This is the resolved form produced by config loading. TOML
/// deserialization uses [`parse::RawConfig`] internally; per-layer
/// `[commands]` sections are folded into `resolved_commands` during
/// merge and the raw form is dropped.
#[derive(Debug, Clone)]
pub struct Config {
    /// Log retention in days (default: 7).
    /// 0 = no persistent logging (cleanup on exit).
    /// -1 = retain logs forever.
    pub log_retention_days: i64,

    /// Language definitions keyed by language ID (e.g., "rust", "python").
    pub language: HashMap<String, LanguageConfig>,

    /// Server definitions keyed by server name.
    pub server: HashMap<String, ServerDef>,

    /// Notification delivery configuration.
    ///
    /// `None` when no source specified `[notifications]`. Use
    /// `unwrap_or_default()` at consumption sites to get the default
    /// threshold (`warn`). Kept as `Option` so layered merge can
    /// distinguish "absent" from "explicitly set to default".
    pub notifications: Option<NotificationConfig>,

    /// Icon theme configuration.
    ///
    /// `None` when no source specified `[icons]`. Absent sections fall
    /// through to the earlier config layer.
    pub icons: Option<IconConfig>,

    /// TUI configuration.
    ///
    /// `None` when no source specified `[tui]`. Absent sections fall
    /// through to the earlier config layer.
    pub tui: Option<TuiConfig>,

    /// Per-tool configuration (budgets, maps options, etc.).
    ///
    /// `None` when no source specified `[tools]`. Absent sections fall
    /// through to the earlier config layer.
    pub tools: Option<ToolsConfig>,

    /// Merged command filter after layered resolution.
    ///
    /// Built incrementally during config loading. `None` when no source
    /// specified `[commands]`. Each layer's fields overwrite when present;
    /// `allow` and `pipeline` are replaced, `deny` entries merge per-command.
    pub resolved_commands: Option<ResolvedCommands>,

    /// Firehose reaping knobs (`[observability]`).
    ///
    /// `None` when no source specified `[observability]`. Bounds JSONL firehose
    /// growth at every level (ticket 01); the defaults require no user action.
    /// Resolve to concrete values via [`Config::reap_policy`].
    pub observability: Option<ReapPolicy>,

    /// Workspace-root policy (`[roots]`), including companion-root derivation.
    ///
    /// `None` when no source specified `[roots]`. User-config only. Read via
    /// [`Config::companion_rules`].
    pub roots: Option<RootsConfig>,

    /// Standalone-linter definitions keyed by linter name (`[linter.*]`).
    ///
    /// The user-level half of the linter feeder (workstream 34 ticket 01). The
    /// effective set for a root is this map unioned with the root's project
    /// `[linter.*]` — see
    /// [`LspClientManager::effective_linters`](crate::lsp::LspClientManager::effective_linters).
    pub linter: HashMap<String, LinterConfig>,
}

/// Icon preset selecting a base set of icons.
#[derive(Debug, Deserialize, Clone, Default)]
#[serde(rename_all = "lowercase")]
pub enum IconPreset {
    /// Safe Unicode symbols that render on any terminal font.
    #[default]
    Unicode,
    /// Nerd Font glyphs (requires a patched font).
    Nerd,
    /// Emoji icons (Unicode 17.0, requires emoji-capable font).
    Emoji,
}

/// Icon theme configuration.
///
/// Set `preset` to choose a base icon set, then override individual icons
/// as needed. Each override replaces the preset default for that slot.
///
/// # Examples
///
/// ```toml
/// [icons]
/// preset = "nerd"
/// ```
///
#[derive(Debug, Deserialize, Clone, Default)]
pub struct IconConfig {
    /// Base icon preset (default: `unicode`).
    #[serde(default)]
    pub preset: IconPreset,
    /// Diagnostic error icon.
    pub diag_error: Option<String>,
    /// Diagnostic warning icon.
    pub diag_warn: Option<String>,
    /// Diagnostic info icon.
    pub diag_info: Option<String>,
    /// Diagnostic ok (clean) icon.
    pub diag_ok: Option<String>,
    /// Search tool icon.
    pub tool_search: Option<String>,
    /// Glob tool icon.
    pub tool_glob: Option<String>,
    /// Default tool icon (fallback).
    pub tool_default: Option<String>,
    /// Workspace expanded icon.
    pub workspace_open: Option<String>,
    /// Workspace collapsed icon.
    pub workspace_closed: Option<String>,
    /// Pinned panel icon.
    pub pinned: Option<String>,
    /// Progress spinner frames (animated).
    pub progress: Option<String>,
    /// Session started event icon.
    pub session_started: Option<String>,
    /// Session shutdown event icon.
    pub session_shutdown: Option<String>,
    /// Server state change event icon.
    pub server_state: Option<String>,
    /// Sed tool icon.
    pub tool_sed: Option<String>,
    /// Language server active icon.
    pub ls_active: Option<String>,
    /// Language server inactive icon.
    pub ls_inactive: Option<String>,
    /// Protocol success icon.
    pub proto_ok: Option<String>,
    /// Protocol error icon.
    pub proto_error: Option<String>,
    /// Request cancelled icon.
    pub cancelled: Option<String>,
    /// Server log info icon (collapsed `window/logMessage` runs at info level).
    pub log_info: Option<String>,
    /// Spinner grow phase frames (plays once at start).
    pub spinner_grow: Option<Vec<String>>,
    /// Spinner cycle phase frames (loops during progress).
    pub spinner_cycle: Option<Vec<String>>,
    /// Spinner done frame (shown on progress end).
    pub spinner_done: Option<String>,
}

/// TUI configuration options.
///
/// Controls the interactive monitor's layout and behavior.
#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "config struct — each field is an independent toggle"
)]
pub struct TuiConfig {
    /// Automatically add new sessions to the grid (default: true).
    pub auto_add_sessions: bool,

    /// Preferred width of the Sessions tree as a fraction of the terminal
    /// (default: 0.25).
    pub sessions_width: f64,

    /// Whether mouse hover changes focus (default: false).
    pub focus_follows_mouse: bool,

    /// Capture full tool output in `ToolResult` events for TUI detail
    /// expansion (default: false). Increases database size.
    pub capture_tool_output: bool,

    /// Keep panels open after a session dies instead of closing them
    /// (default: false). Dead panels show the session ID with dimmed
    /// styling. When false, panels are closed on the next liveness check.
    pub keep_dead_panels: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            auto_add_sessions: true,
            sessions_width: 0.25,
            focus_follows_mouse: false,
            capture_tool_output: false,
            keep_dead_panels: false,
        }
    }
}

/// Default diagnostics preview budget.
const fn default_diagnostics_per_page() -> usize {
    50
}

/// Default dirty-severity threshold for `catenary diagnostics`.
fn default_diagnostics_severity() -> String {
    "error".to_string()
}

/// Per-tool configuration.
///
/// Configures output budgets and tool-specific options. Each tool has its
/// own section under `[tools]`:
///
/// ```toml
/// [tools.grep]
/// budget = 4000
///
/// [tools.glob]
/// budget = 2000
/// outline_threshold = 200
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// Grep tool configuration.
    pub grep: GrepConfig,
    /// Glob tool configuration.
    pub glob: GlobConfig,
    /// Single-shot preview budget for `catenary diagnostics`. When the run
    /// produces more than this many diagnostics, the preview shows the first
    /// N (errors before warnings, so a truncation never hides an error behind
    /// a warning) and the **complete** set is written to a per-session file
    /// under the runtime dir, named in a trailing `… N more — full report at
    /// <path>` line. Not a replayable page — the set clears on run. Default: 50.
    #[serde(default = "default_diagnostics_per_page")]
    pub diagnostics_per_page: usize,
    /// Minimum diagnostic severity that marks a `catenary diagnostics` run
    /// "dirty" (exit code 1) — one of `error`, `warning`, `info`, `hint`.
    /// Default `error`, so the exit code means "does it compile": only
    /// error-severity diagnostics gate, and a server's constant unused-var
    /// warnings don't block every test run. Warnings still print; they just
    /// exit 0. An unrecognized value falls back to `error`.
    #[serde(default = "default_diagnostics_severity")]
    pub diagnostics_severity: String,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            grep: GrepConfig::default(),
            glob: GlobConfig::default(),
            diagnostics_per_page: default_diagnostics_per_page(),
            diagnostics_severity: default_diagnostics_severity(),
        }
    }
}

impl ToolsConfig {
    /// The LSP severity (1=Error … 4=Hint) at or above which a diagnostic
    /// marks a `catenary diagnostics` run dirty (exit code 1).
    ///
    /// Parses [`Self::diagnostics_severity`], falling back to
    /// [`SEVERITY_ERROR`](crate::filter::SEVERITY_ERROR) for an unrecognized
    /// value.
    #[must_use]
    pub fn dirty_severity(&self) -> u8 {
        crate::filter::parse_severity(&self.diagnostics_severity)
            .unwrap_or(crate::filter::SEVERITY_ERROR)
    }

    /// The single-shot diagnostics preview budget, clamped to a minimum of 1.
    #[must_use]
    pub const fn diagnostics_budget(&self) -> usize {
        if self.diagnostics_per_page == 0 {
            1
        } else {
            self.diagnostics_per_page
        }
    }

    /// Clamp budgets to their minimum values, warning on adjustment.
    pub(crate) fn clamp_budgets(&mut self) {
        if self.grep.budget < 2000 {
            tracing::warn!(
                budget = self.grep.budget,
                min = 2000,
                "grep budget below minimum, clamping to 2000",
            );
            self.grep.budget = 2000;
        }
        if self.glob.budget < 1000 {
            tracing::warn!(
                budget = self.glob.budget,
                min = 1000,
                "glob budget below minimum, clamping to 1000",
            );
            self.glob.budget = 1000;
        }
        if self.diagnostics_per_page == 0 {
            tracing::warn!(min = 1, "diagnostics_per_page cannot be 0, clamping to 1",);
            self.diagnostics_per_page = 1;
        }
    }
}

/// Grep tool configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GrepConfig {
    /// Output budget in characters. Default: 4000, min: 2000.
    pub budget: u32,
}

impl Default for GrepConfig {
    fn default() -> Self {
        Self { budget: 4000 }
    }
}

/// Glob tool configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct GlobConfig {
    /// Output budget in characters. Default: 2000, min: 1000.
    pub budget: u32,
    /// Minimum line count for defensive outlines. Default: 200.
    pub outline_threshold: usize,
    /// Glob patterns whose outlines are suppressed from automatic display.
    /// Symbols remain available via `into`.
    pub outline_suppress: Vec<String>,
}

impl Default for GlobConfig {
    fn default() -> Self {
        Self {
            budget: 2000,
            outline_threshold: 200,
            outline_suppress: Vec::new(),
        }
    }
}

pub(crate) const fn default_log_retention_days() -> i64 {
    7
}

impl Config {
    /// Load configuration from standard paths or a specific file.
    ///
    /// Sources are loaded in order, with later sources overriding earlier ones:
    /// 1. User config (`~/.config/catenary/config.toml`)
    /// 2. Explicit file (if provided via `CATENARY_CONFIG`)
    /// 3. Environment variable overrides
    ///
    /// Project-local config (`.catenary.toml`) is not loaded here — it is
    /// discovered per-root by [`load_project_config`] and stored on
    /// `LspClientManager`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A configuration file exists but cannot be read or parsed.
    /// - A file uses the deprecated `[server.*]` key without `[language.*]`.
    /// - A `[language.*]` entry uses the removed `inherit` field.
    /// - A concrete language entry has no `servers` list.
    pub fn load() -> Result<Self> {
        parse::load()
    }

    /// Parse and validate configuration without side effects.
    ///
    /// Reads config sources, parses TOML, and runs validation. Returns
    /// `Ok(())` if the config is valid, or an error describing what's wrong.
    /// Does not spawn servers, scan the filesystem, or access the database.
    ///
    /// # Errors
    ///
    /// Returns an error if any config source cannot be read or parsed, or
    /// if validation finds issues (missing servers, broken inherits, etc.).
    pub fn check() -> Result<()> {
        let _ = Self::load()?;
        Ok(())
    }

    /// Load configuration from an explicit list of file paths.
    ///
    /// Sources are merged in order (later overrides earlier). Environment
    /// variable overrides and validation are applied after merging.
    #[cfg(test)]
    pub(crate) fn load_from_sources(sources: &[std::path::PathBuf]) -> Result<Self> {
        parse::load_from_sources(sources)
    }

    /// Apply environment variable overrides for supported keys.
    fn apply_env_overrides(&mut self) {
        parse::apply_env_overrides(self);
    }

    /// Validate the merged config, returning all errors found.
    ///
    /// Returns an empty vec when the config is valid.
    #[must_use]
    pub fn validate(&self) -> Vec<String> {
        validate::validate(self)
    }

    /// Look up the configuration for a language key.
    #[must_use]
    pub fn resolve_language(&self, key: &str) -> Option<&LanguageConfig> {
        self.language.get(key)
    }

    /// Resolve the firehose reaping policy, falling back to defaults when no
    /// `[observability]` section was configured (ticket 01).
    #[must_use]
    pub fn reap_policy(&self) -> ReapPolicy {
        self.observability.unwrap_or_default()
    }

    /// Returns the configured companion-root rules, or `None` when the feature
    /// is off (`[roots.companions]` absent).
    ///
    /// Used by the MCP root callback to expand a connection's declared roots
    /// with their derived companions (workstream 29).
    #[must_use]
    pub fn companion_rules(&self) -> Option<&CompanionRules> {
        self.roots.as_ref()?.companions.as_ref()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_retention_days: default_log_retention_days(),
            language: HashMap::new(),
            server: HashMap::new(),
            notifications: None,
            icons: None,
            tui: None,
            tools: None,
            resolved_commands: None,
            observability: None,
            roots: None,
            linter: HashMap::new(),
        }
    }
}

impl Config {
    /// Returns a default config with the embedded classification data loaded.
    ///
    /// This is equivalent to loading from no sources — only the embedded
    /// `defaults/languages.toml` is applied.
    #[must_use]
    pub fn default_with_classification() -> Self {
        parse::load_from_sources(&[]).unwrap_or_default()
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_config_load_local() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer-local"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        assert_eq!(
            config
                .language
                .get("rust")
                .expect("rust language config")
                .servers,
            Some(vec![ServerBinding::new("rust-analyzer")]),
        );

        Ok(())
    }

    #[test]
    fn test_config_load_linter() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[linter.yamllint]
command = "yamllint"
args = ["-f", "parsable"]
patterns = ["**/*.{yml,yaml}"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;
        let yl = config.linter.get("yamllint").expect("yamllint linter");
        assert_eq!(yl.command, "yamllint");
        // Routing globs are compiled after validation.
        assert!(yl.matches(std::path::Path::new("a/b.yaml")));
        assert!(!yl.matches(std::path::Path::new("a/b.txt")));

        Ok(())
    }

    #[test]
    fn test_config_linter_invalid_glob_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            "[linter.x]\ncommand = \"x\"\npatterns = [\"[bad\"]\n",
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        let err = format!("{:#}", result.expect_err("invalid glob should error"));
        assert!(
            err.contains("invalid glob") && err.contains("patterns"),
            "error should mention the invalid linter glob: {err}",
        );
    }

    #[test]
    fn test_config_linter_empty_command_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(&config_path, "[linter.x]\npatterns = [\"**/*.sh\"]\n").expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        let err = format!("{:#}", result.expect_err("empty command should error"));
        assert!(
            err.contains("empty `command`"),
            "error should mention the empty linter command: {err}",
        );
    }

    #[test]
    fn test_old_server_key_hard_error() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust]
command = "rust-analyzer"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("deprecated"),
            "error should mention deprecated: {err}",
        );
    }

    #[test]
    fn test_server_def_parsing() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"
args = ["--log-level", "info"]

[server.clangd]
command = "clangd"
args = ["--background-index"]
settings = { checkOnSave = true }

[language.rust]
servers = ["rust-analyzer"]

[language.c]
servers = ["clangd"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        assert!(config.language.contains_key("rust"));
        // User defs override built-in defaults — verify the overrides took effect.
        let ra = config
            .server
            .get("rust-analyzer")
            .expect("rust-analyzer server def");
        assert_eq!(ra.command, "rust-analyzer");
        assert_eq!(ra.args, vec!["--log-level", "info"]);

        let clangd = config.server.get("clangd").expect("clangd server def");
        assert_eq!(clangd.command, "clangd");
        assert_eq!(clangd.args, vec!["--background-index"]);
        assert!(clangd.settings.is_some());

        Ok(())
    }

    #[test]
    fn test_both_server_and_language_valid() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        // This should succeed — new format with both sections
        let config = Config::load_from_sources(&[config_path])?;
        assert!(config.server.contains_key("rust-analyzer"));
        assert!(config.language.contains_key("rust"));

        Ok(())
    }

    #[test]
    fn test_server_def_merge() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let source1 = dir.path().join("source1.toml");
        fs::write(
            &source1,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[server.clangd]
command = "clangd"
args = ["--background-index"]

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let source2 = dir.path().join("source2.toml");
        fs::write(
            &source2,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[server.clangd]
command = "clangd"
args = ["--background-index", "--clang-tidy"]
settings = { checkOnSave = true }

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let config = Config::load_from_sources(&[source1, source2])?;

        let clangd = config.server.get("clangd").expect("clangd server def");
        assert_eq!(clangd.args, vec!["--background-index", "--clang-tidy"]);
        assert!(clangd.settings.is_some());

        Ok(())
    }

    #[test]
    fn test_server_def_validation() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[server.bad-server]
command = ""

[language.rust]
servers = ["rust-analyzer"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("empty") && err.contains("command"),
            "error should mention empty command: {err}",
        );
    }

    #[test]
    fn test_inherit_field_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.tsserver]
command = "typescript-language-server"

[language.typescript]
servers = ["tsserver"]

[language.typescriptreact]
inherit = "typescript"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("inherit") && err.contains("removed"),
            "error should mention removed inherit field: {err}",
        );
    }

    #[test]
    fn test_concrete_without_servers_or_classification_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        // Entry with only diagnostics but no servers and no classification
        // should be rejected.
        fs::write(
            &config_path,
            r"
[language.custom]
diagnostics = false
",
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("servers") || err.contains("classification"),
            "error should mention servers or classification: {err}",
        );
    }

    #[test]
    fn test_resolve_language_direct() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.tsserver]
command = "typescript-language-server"
args = ["--stdio"]

[language.typescript]
servers = ["tsserver"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        let resolved = config
            .resolve_language("typescript")
            .expect("should resolve");
        assert_eq!(resolved.servers, Some(vec![ServerBinding::new("tsserver")]));

        // typescriptreact exists from defaults with built-in server binding
        let tsx = config
            .resolve_language("typescriptreact")
            .expect("should exist from defaults");
        assert_eq!(
            tsx.servers,
            Some(vec![ServerBinding::new("typescript-ls")]),
            "TSX should have built-in typescript-ls binding"
        );

        // Truly unconfigured language returns None
        assert!(config.resolve_language("brainfuck").is_none());

        Ok(())
    }

    #[test]
    fn test_empty_config() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "")?;

        let config = Config::load_from_sources(&[config_path])?;
        assert_eq!(config.log_retention_days, 7);
        // Default classification entries are loaded
        assert!(!config.language.is_empty());
        // Built-in server definitions are loaded
        assert!(
            !config.server.is_empty(),
            "built-in server defaults should be loaded"
        );
        assert!(
            config.server.contains_key("rust-analyzer"),
            "rust-analyzer should be in built-in defaults"
        );

        Ok(())
    }

    #[test]
    fn test_merge_later_source_overrides() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let local_config_path = dir.path().join(".catenary.toml");
        fs::write(
            &local_config_path,
            r#"
log_retention_days = 14

[server.rust-analyzer]
command = "rust-analyzer-local"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let explicit_path = dir.path().join("explicit.toml");
        fs::write(
            &explicit_path,
            r"
log_retention_days = 30
",
        )?;

        let config = Config::load_from_sources(&[local_config_path, explicit_path])?;

        assert_eq!(config.log_retention_days, 30);
        assert!(config.language.contains_key("rust"));

        Ok(())
    }

    #[test]
    fn test_new_format_roundtrip() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"
args = ["--log-level", "info"]
min_severity = "warning"

[server.clangd]
command = "clangd"
args = ["--background-index"]

[language.rust]
servers = ["rust-analyzer"]

[language.c]
servers = ["clangd"]

[language.cpp]
servers = ["clangd"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        // Server defs
        let ra = config
            .server
            .get("rust-analyzer")
            .expect("rust-analyzer server def");
        assert_eq!(ra.command, "rust-analyzer");
        assert_eq!(ra.args, vec!["--log-level", "info"]);
        assert_eq!(ra.min_severity.as_deref(), Some("warning"));

        // Language entries
        let rust = config.language.get("rust").expect("rust config");
        assert_eq!(
            rust.servers,
            Some(vec![ServerBinding::new("rust-analyzer")])
        );

        let c = config.language.get("c").expect("c config");
        assert_eq!(c.servers, Some(vec![ServerBinding::new("clangd")]));

        let cpp = config.language.get("cpp").expect("cpp config");
        assert_eq!(cpp.servers, Some(vec![ServerBinding::new("clangd")]));

        Ok(())
    }

    #[test]
    fn test_inline_command_hard_error() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.rust]
command = "rust-analyzer"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("command") && err.contains("[server.*]"),
            "error should mention server definition migration: {err}",
        );
    }

    #[test]
    fn test_inline_single_file_hard_error() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.shellscript]
single_file = true
servers = ["bash-language-server"]

[server.bash-language-server]
command = "bash-language-server"
args = ["start"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("single_file") && err.contains("[server.*]"),
            "error should mention server definition migration: {err}",
        );
    }

    /// Ensures every config-visible `ServerDef` field is listed in
    /// `SERVER_DEF_KEYS`. Fails when a field is added to the struct
    /// without updating the constant.
    #[test]
    fn test_server_def_keys_sync() {
        use std::collections::HashMap;

        let def = ServerDef {
            command: "x".into(),
            args: vec!["a".into()],
            env: Some(HashMap::from([("K".into(), "V".into())])),
            initialization_options: Some(serde_json::json!({})),
            settings: Some(serde_json::json!({})),
            min_severity: Some("error".into()),
            single_file: true,
            file_patterns: vec!["*.rs".into()],
            diagnostic_precedence: Some(crate::config::DiagnosticPrecedence {
                advisory_sources: vec!["rust-analyzer".into()],
                authoritative_sources: vec!["rustc".into()],
                code_pattern: Some("^E[0-9]+$".into()),
                compiled_code_pattern: None,
            }),
            compiled_patterns: Vec::new(),
        };

        let value = toml::Value::try_from(&def).expect("serialize ServerDef");
        let table = value.as_table().expect("should be a table");

        for key in table.keys() {
            assert!(
                SERVER_DEF_KEYS.contains(&key.as_str()),
                "ServerDef field `{key}` missing from SERVER_DEF_KEYS — \
                 add it so misplaced-field detection catches it on [language.*]",
            );
        }

        // Reverse check: every key in SERVER_DEF_KEYS should appear in
        // the serialized output (catches stale entries).
        for key in SERVER_DEF_KEYS {
            assert!(
                table.contains_key(*key),
                "SERVER_DEF_KEYS lists `{key}` but ServerDef has no such field — \
                 remove it from the constant",
            );
        }
    }

    #[test]
    fn test_undefined_server_ref() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.rust]
servers = ["nonexistent-server"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("nonexistent-server"),
            "error should mention the undefined server: {err}",
        );
    }

    #[test]
    fn test_explicit_empty_servers_clears_builtin() -> anyhow::Result<()> {
        // User writes `servers = []` to actively clear the built-in
        // server binding for a language. The `Some([])` from deserialization
        // replaces the default `Some(["rust-analyzer"])` during merge.
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r"
[language.rust]
servers = []
",
        )?;

        let config = Config::load_from_sources(&[config_path])?;
        let rust = config.language.get("rust").expect("rust config");
        assert_eq!(
            rust.servers,
            Some(vec![]),
            "explicit servers = [] should clear built-in binding"
        );
        assert!(rust.extensions.is_some());

        Ok(())
    }

    #[test]
    fn test_resolve_language_borrows() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.tsserver]
command = "typescript-language-server"
min_severity = "warning"

[language.typescript]
servers = ["tsserver"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        // Verify the returned config borrows from the map
        let resolved = config
            .resolve_language("typescript")
            .expect("should resolve");
        assert_eq!(resolved.servers, Some(vec![ServerBinding::new("tsserver")]));

        let server = config.server.get("tsserver").expect("tsserver def");
        assert_eq!(server.min_severity.as_deref(), Some("warning"));

        Ok(())
    }

    #[test]
    fn test_parse_server_specs_single() {
        let results = parse::parse_server_specs("rust:rust-analyzer --log-level info");
        assert_eq!(results.len(), 1);

        let (lang, server_def, lang_config) = &results[0];
        assert_eq!(lang, "rust");
        assert_eq!(server_def.command, "rust-analyzer");
        assert_eq!(server_def.args, vec!["--log-level", "info"]);
        assert_eq!(lang_config.servers, Some(vec![ServerBinding::new("rust")]));
    }

    #[test]
    fn test_parse_server_specs_multiple() {
        let results =
            parse::parse_server_specs("rust:rust-analyzer;python:pyright --stdio;c:clangd");
        assert_eq!(results.len(), 3);

        assert_eq!(results[0].0, "rust");
        assert_eq!(results[0].1.command, "rust-analyzer");
        assert!(results[0].1.args.is_empty());

        assert_eq!(results[1].0, "python");
        assert_eq!(results[1].1.command, "pyright");
        assert_eq!(results[1].1.args, vec!["--stdio"]);

        assert_eq!(results[2].0, "c");
        assert_eq!(results[2].1.command, "clangd");
    }

    #[test]
    fn test_parse_server_specs_empty_and_whitespace() {
        let results = parse::parse_server_specs("  ; ;rust:ra;  ");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "rust");
        assert_eq!(results[0].1.command, "ra");
    }

    #[test]
    fn test_resolve_language_servers() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.tsserver]
command = "typescript-language-server"

[language.typescript]
servers = ["tsserver"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;

        let resolved = config
            .resolve_language("typescript")
            .expect("should resolve");
        assert_eq!(resolved.servers, Some(vec![ServerBinding::new("tsserver")]));

        // Unconfigured language returns None
        assert!(config.resolve_language("unknown").is_none());

        Ok(())
    }

    #[test]
    fn test_config_check_valid() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        // check() should succeed for a valid config
        let config = Config::load_from_sources(&[config_path]);
        assert!(config.is_ok());

        Ok(())
    }

    #[test]
    fn test_config_check_invalid_old_format() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[language.rust]
command = "rust-analyzer"
"#,
        )
        .expect("write config");

        // check() should fail for old inline format
        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("[server.*]"),
            "error should mention server migration: {err}",
        );
    }

    #[test]
    fn test_config_check_fast() {
        // Config loading must use negligible CPU — regression guard against
        // accidental network calls or O(n²) parsing.  Measures process CPU
        // time (centiseconds, 100 Hz) instead of wall-clock to avoid flakes
        // under parallel test load.
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "").expect("write config");

        let pid = std::process::id();
        let before = catenary_proc::sample(pid).expect("sample before");
        let _ = Config::load_from_sources(&[config_path]);
        let after = catenary_proc::sample(pid).expect("sample after");

        let cpu_ticks = (after.utime + after.stime) - (before.utime + before.stime);
        // 100 ticks = 1s CPU time — generous enough to avoid flakes
        // under parallel cargo-mutants load while still catching
        // catastrophic regressions (accidental network calls, O(n²)).
        assert!(
            cpu_ticks <= 100,
            "config check used {cpu_ticks} CPU ticks (centiseconds), expected <= 100",
        );
    }

    #[test]
    fn notification_config_default() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "")?;

        let config = Config::load_from_sources(&[config_path])?;
        assert!(config.notifications.is_none());
        assert_eq!(
            config.notifications.unwrap_or_default().threshold,
            SeverityConfig::Warn,
        );

        Ok(())
    }

    #[test]
    fn roots_companions_absent_is_off() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "")?;

        let config = Config::load_from_sources(&[path])?;
        assert!(config.roots.is_none());
        assert!(config.companion_rules().is_none());

        Ok(())
    }

    #[test]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "`{root}Internal` is a companion-template placeholder in TOML, not a format arg"
    )]
    fn roots_companions_parses() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[roots.companions]\n\
             \"*\" = \"{root}Internal\"\n\
             \"~/Projects/homelab\" = \"~/.local/share/chezmoi\"\n",
        )?;

        let config = Config::load_from_sources(&[path])?;
        let rules = config.companion_rules().expect("companions configured");
        assert!(!rules.is_empty());

        Ok(())
    }

    #[test]
    fn roots_rejects_unknown_field() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "[roots]\nbogus = true\n").expect("write");

        assert!(Config::load_from_sources(&[path]).is_err());
    }

    #[test]
    fn notification_config_parses_all_levels() -> anyhow::Result<()> {
        let dir = tempdir()?;
        for (toml_val, expected) in [
            ("debug", SeverityConfig::Debug),
            ("info", SeverityConfig::Info),
            ("warn", SeverityConfig::Warn),
            ("error", SeverityConfig::Error),
        ] {
            let path = dir.path().join(format!("{toml_val}.toml"));
            fs::write(
                &path,
                format!("[notifications]\nthreshold = \"{toml_val}\"\n"),
            )?;
            let config = Config::load_from_sources(&[path])?;
            assert_eq!(
                config.notifications.expect("should be Some").threshold,
                expected,
            );
        }

        Ok(())
    }

    #[test]
    fn notification_config_rejects_unknown_key() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "[notifications]\nfoo = \"bar\"\n").expect("write");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
    }

    #[test]
    fn notification_config_project_overrides_user() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let user = dir.path().join("user.toml");
        fs::write(&user, "[notifications]\nthreshold = \"warn\"\n")?;

        let project = dir.path().join("project.toml");
        fs::write(&project, "[notifications]\nthreshold = \"info\"\n")?;

        let config = Config::load_from_sources(&[user, project])?;
        assert_eq!(
            config.notifications.expect("should be Some").threshold,
            SeverityConfig::Info,
        );

        Ok(())
    }

    #[test]
    fn notification_config_project_absent_falls_through() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let user = dir.path().join("user.toml");
        fs::write(&user, "[notifications]\nthreshold = \"error\"\n")?;

        let project = dir.path().join("project.toml");
        fs::write(&project, "")?;

        let config = Config::load_from_sources(&[user, project])?;
        // Project omits [notifications] entirely — user's value is preserved.
        assert_eq!(
            config.notifications.unwrap_or_default().threshold,
            SeverityConfig::Error,
        );

        Ok(())
    }

    #[test]
    fn notification_config_desktop_defaults_to_none() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[notifications]\nthreshold = \"warn\"\n")?;

        let config = Config::load_from_sources(&[path])?;
        let notif = config.notifications.expect("should be Some");
        assert!(
            notif.desktop.is_none(),
            "desktop should be None when omitted"
        );
        // Consuming code uses unwrap_or(true).
        assert!(notif.desktop.unwrap_or(true));

        Ok(())
    }

    #[test]
    fn notification_config_desktop_false() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[notifications]\ndesktop = false\n")?;

        let config = Config::load_from_sources(&[path])?;
        let notif = config.notifications.expect("should be Some");
        assert_eq!(notif.desktop, Some(false));

        Ok(())
    }

    #[test]
    fn severity_config_converts_to_logging_severity() {
        use crate::logging::Severity;

        assert_eq!(Severity::from(SeverityConfig::Debug), Severity::Debug);
        assert_eq!(Severity::from(SeverityConfig::Info), Severity::Info);
        assert_eq!(Severity::from(SeverityConfig::Warn), Severity::Warn);
        assert_eq!(Severity::from(SeverityConfig::Error), Severity::Error);
    }

    #[test]
    fn test_bare_string_binding() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = ["foo"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert_eq!(lc.servers().len(), 1);
        assert_eq!(lc.servers()[0].name, "foo");
        assert!(lc.servers()[0].diagnostics);

        Ok(())
    }

    #[test]
    fn test_inline_table_binding() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = [{ name = "foo", diagnostics = false }]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert_eq!(lc.servers().len(), 1);
        assert_eq!(lc.servers()[0].name, "foo");
        assert!(!lc.servers()[0].diagnostics);

        Ok(())
    }

    #[test]
    fn test_mixed_binding() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.alpha]
command = "alpha-server"

[server.beta]
command = "beta-server"

[language.test]
servers = ["alpha", { name = "beta", diagnostics = false }]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert_eq!(lc.servers().len(), 2);
        assert_eq!(
            lc.servers,
            Some(vec![
                ServerBinding::new("alpha"),
                ServerBinding {
                    name: "beta".to_string(),
                    diagnostics: false,
                    disabled_methods: Vec::new(),
                },
            ]),
        );

        Ok(())
    }

    #[test]
    fn test_unknown_binding_key_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = [{ name = "foo", typo = true }]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("typo"),
            "error should mention the unknown key: {err}",
        );
    }

    #[test]
    fn test_disabled_methods_parsed() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.alpha]
command = "alpha-server"

[language.test]
servers = [{ name = "alpha", disabled_methods = ["textDocument/references"] }]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert_eq!(lc.servers().len(), 1);
        assert_eq!(
            lc.servers()[0].disabled_methods,
            vec![DispatchMethod::References]
        );
        assert!(lc.servers()[0].is_method_disabled(DispatchMethod::References));
        assert!(!lc.servers()[0].is_method_disabled(DispatchMethod::Implementation));

        Ok(())
    }

    #[test]
    fn test_disabled_methods_unknown_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.alpha]
command = "alpha-server"

[language.test]
servers = [{ name = "alpha", disabled_methods = ["textDocument/typo"] }]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("typo"),
            "error should mention the unknown method: {err}",
        );
    }

    #[test]
    fn test_disabled_methods_diagnostic_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.alpha]
command = "alpha-server"

[language.test]
servers = [{ name = "alpha", disabled_methods = ["textDocument/diagnostic"] }]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("diagnostics = false"),
            "error should guide toward diagnostics flag: {err}",
        );
    }

    #[test]
    fn test_disabled_methods_default_empty() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = ["foo"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert!(lc.servers()[0].disabled_methods.is_empty());

        Ok(())
    }

    #[test]
    fn test_language_diagnostics_default() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = ["foo"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("test").expect("test language");
        assert!(lc.diagnostics);

        Ok(())
    }

    #[test]
    fn test_language_diagnostics_false() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.md-server]
command = "md-server"

[language.markdown]
servers = ["md-server"]
diagnostics = false
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let lc = config.language.get("markdown").expect("markdown language");
        assert!(!lc.diagnostics);

        Ok(())
    }

    #[test]
    fn test_diagnostics_enabled_and_logic() {
        // language true, binding true → true
        let lc = LanguageConfig {
            servers: Some(vec![ServerBinding::new("s")]),
            ..LanguageConfig::default()
        };
        assert!(lc.diagnostics_enabled("s"));

        // language false, binding true → false
        let lc = LanguageConfig {
            servers: Some(vec![ServerBinding::new("s")]),
            diagnostics: false,
            ..LanguageConfig::default()
        };
        assert!(!lc.diagnostics_enabled("s"));

        // language true, binding false → false
        let lc = LanguageConfig {
            servers: Some(vec![ServerBinding {
                name: "s".to_string(),
                diagnostics: false,
                disabled_methods: Vec::new(),
            }]),
            ..LanguageConfig::default()
        };
        assert!(!lc.diagnostics_enabled("s"));

        // language false, binding false → false
        let lc = LanguageConfig {
            servers: Some(vec![ServerBinding {
                name: "s".to_string(),
                diagnostics: false,
                disabled_methods: Vec::new(),
            }]),
            diagnostics: false,
            ..LanguageConfig::default()
        };
        assert!(!lc.diagnostics_enabled("s"));
    }

    #[test]
    fn test_diagnostics_enabled_unknown_server() {
        let lc = LanguageConfig {
            servers: Some(vec![ServerBinding::new("known")]),
            ..LanguageConfig::default()
        };
        assert!(!lc.diagnostics_enabled("unknown"));
    }

    #[test]
    fn test_env_var_creates_binding() {
        let results = parse::parse_server_specs("rust:rust-analyzer");
        assert_eq!(results.len(), 1);

        let (_, _, lang_config) = &results[0];
        assert_eq!(lang_config.servers().len(), 1);
        assert_eq!(lang_config.servers()[0].name, "rust");
        assert!(lang_config.servers()[0].diagnostics);
    }

    #[test]
    fn test_min_severity_on_server() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"
min_severity = "warning"

[language.test]
servers = ["foo"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let server = config.server.get("foo").expect("foo server def");
        assert_eq!(server.min_severity.as_deref(), Some("warning"));

        Ok(())
    }

    #[test]
    fn test_min_severity_on_language_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.rust]
servers = ["foo"]
min_severity = "warning"
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("min_severity") && err.contains("[server.*]"),
            "error should mention moving min_severity to server: {err}",
        );
    }

    #[test]
    fn test_min_severity_absent() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = ["foo"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let server = config.server.get("foo").expect("foo server def");
        assert!(server.min_severity.is_none());

        Ok(())
    }

    // --- Classification fields and default config ---

    #[test]
    fn test_user_config_inherits_defaults() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let rust = config.language.get("rust").expect("rust config");
        // servers comes from user config
        assert_eq!(
            rust.servers,
            Some(vec![ServerBinding::new("rust-analyzer")])
        );
        // extensions inherited from defaults
        assert_eq!(
            rust.extensions.as_deref(),
            Some(["rs"].map(str::to_string).as_slice()),
        );

        Ok(())
    }

    #[test]
    fn test_user_config_overrides_classification() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.bash-ls]
command = "bash-language-server"

[language.shellscript]
servers = ["bash-ls"]
filenames = ["PKGBUILD", "APKBUILD"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let shell = config.language.get("shellscript").expect("shellscript");
        // filenames overridden by user
        assert_eq!(
            shell.filenames.as_deref(),
            Some(["PKGBUILD", "APKBUILD"].map(str::to_string).as_slice()),
        );
        // extensions preserved from defaults (user didn't override)
        assert!(shell.extensions.is_some());
        assert!(
            shell
                .extensions
                .as_ref()
                .expect("extensions")
                .contains(&"sh".to_string()),
        );

        Ok(())
    }

    #[test]
    fn test_file_patterns_on_server() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.pkgbuild-ls]
command = "pkgbuild-ls"
file_patterns = ["PKGBUILD"]

[language.shellscript]
servers = ["pkgbuild-ls"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let server = config.server.get("pkgbuild-ls").expect("server def");
        assert_eq!(server.file_patterns, vec!["PKGBUILD"]);

        Ok(())
    }

    #[test]
    fn test_file_patterns_invalid_glob() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.bad]
command = "bad-server"
file_patterns = ["[invalid"]

[language.test]
servers = ["bad"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("invalid") && err.contains("glob"),
            "error should mention invalid glob: {err}",
        );
    }

    #[test]
    fn test_file_patterns_empty_string() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[server.bad]
command = "bad-server"
file_patterns = [""]

[language.test]
servers = ["bad"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("empty"),
            "error should mention empty string: {err}",
        );
    }

    #[test]
    fn test_classification_empty_extension_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[language.custom]
extensions = ["rs", ""]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("empty") && err.contains("extensions"),
            "error should mention empty extensions: {err}",
        );
    }

    #[test]
    fn test_field_level_merge() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let base = dir.path().join("base.toml");
        fs::write(
            &base,
            r#"
[server.foo]
command = "foo-server"

[language.test]
servers = ["foo"]
extensions = ["abc"]
filenames = ["TestFile"]
"#,
        )?;

        let overlay = dir.path().join("overlay.toml");
        fs::write(
            &overlay,
            r#"
[language.test]
extensions = ["xyz"]
"#,
        )?;

        let config = Config::load_from_sources(&[base, overlay])?;
        let lc = config.language.get("test").expect("test language");
        // extensions replaced by overlay
        assert_eq!(
            lc.extensions.as_deref(),
            Some(["xyz"].map(str::to_string).as_slice()),
        );
        // filenames preserved (overlay didn't set them)
        assert_eq!(
            lc.filenames.as_deref(),
            Some(["TestFile"].map(str::to_string).as_slice()),
        );
        // servers preserved (overlay didn't mention servers — None preserves)
        assert_eq!(lc.servers, Some(vec![ServerBinding::new("foo")]));

        Ok(())
    }

    // --- Per-tool config (tools.*) ---

    #[test]
    fn test_default_budgets() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "")?;

        let config = Config::load_from_sources(&[path])?;
        let tools = config.tools.unwrap_or_default();
        assert_eq!(tools.grep.budget, 4000);
        assert_eq!(tools.glob.budget, 2000);

        Ok(())
    }

    #[test]
    fn test_custom_grep_budget() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[tools.grep]\nbudget = 8000\n")?;

        let config = Config::load_from_sources(&[path])?;
        let tools = config.tools.expect("tools should be Some");
        assert_eq!(tools.grep.budget, 8000);

        Ok(())
    }

    #[test]
    fn test_minimum_grep_budget() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[tools.grep]\nbudget = 500\n")?;

        let config = Config::load_from_sources(&[path])?;
        let tools = config.tools.expect("tools should be Some");
        assert_eq!(tools.grep.budget, 2000);

        Ok(())
    }

    #[test]
    fn test_minimum_glob_budget() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[tools.glob]\nbudget = 500\n")?;

        let config = Config::load_from_sources(&[path])?;
        let tools = config.tools.expect("tools should be Some");
        assert_eq!(tools.glob.budget, 1000);

        Ok(())
    }

    #[test]
    fn test_glob_outline_threshold() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[tools.glob]\noutline_threshold = 500\n")?;

        let config = Config::load_from_sources(&[path])?;
        let tools = config.tools.expect("tools should be Some");
        assert_eq!(tools.glob.outline_threshold, 500);

        Ok(())
    }

    #[test]
    fn test_glob_outline_suppress() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[tools.glob]\noutline_suppress = [\"**/*.json\", \"**/fixtures/**\"]\n",
        )?;

        let config = Config::load_from_sources(&[path])?;
        let tools = config.tools.expect("tools should be Some");
        assert_eq!(tools.glob.outline_suppress.len(), 2);

        Ok(())
    }

    #[test]
    fn test_missing_tools_section() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "log_retention_days = 14\n")?;

        let config = Config::load_from_sources(&[path])?;
        assert!(config.tools.is_none());
        let tools = config.tools.unwrap_or_default();
        assert_eq!(tools.grep.budget, 4000);
        assert_eq!(tools.glob.budget, 2000);

        Ok(())
    }

    #[test]
    fn test_partial_tools() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "[tools.grep]\nbudget = 6000\n")?;

        let config = Config::load_from_sources(&[path])?;
        let tools = config.tools.expect("tools should be Some");
        assert_eq!(tools.grep.budget, 6000);
        // glob uses defaults
        assert_eq!(tools.glob.budget, 2000);

        Ok(())
    }

    // --- Commands config ---

    #[test]
    fn commands_config_parses() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[commands]
build = "make"
allow = ["git", "gh", "cp"]
pipeline = ["grep", "head", "tail"]

[commands.deny]
git = ["grep", "ls-files"]
"#,
        )?;

        let config = Config::load_from_sources(&[path])?;
        let resolved = config
            .resolved_commands
            .expect("resolved_commands should be Some");
        assert_eq!(resolved.default_build, vec!["make"]);
        assert_eq!(resolved.allow.len(), 3);
        assert!(resolved.allow.contains("git"));
        assert!(resolved.allow.contains("gh"));
        assert!(resolved.allow.contains("cp"));
        assert_eq!(resolved.pipeline.len(), 3);
        assert!(resolved.pipeline.contains("grep"));
        let git_deny = resolved.deny.get("git").expect("git deny");
        assert!(git_deny.contains("grep"));
        assert!(git_deny.contains("ls-files"));

        Ok(())
    }

    #[test]
    fn commands_config_absent() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(&path, "")?;

        let config = Config::load_from_sources(&[path])?;
        assert!(config.resolved_commands.is_none());

        Ok(())
    }

    #[test]
    fn commands_project_allow_replaces_user() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let user = dir.path().join("user.toml");
        fs::write(
            &user,
            r#"
[commands]
allow = ["git", "gh", "cp"]
pipeline = ["grep"]
"#,
        )?;

        let project = dir.path().join("project.toml");
        fs::write(
            &project,
            r#"
[commands]
allow = ["git", "gh", "kubectl"]
"#,
        )?;

        let config = Config::load_from_sources(&[user, project])?;
        let resolved = config
            .resolved_commands
            .expect("resolved_commands should be Some");
        // Project replaces user's allow list
        assert!(resolved.allow.contains("git"));
        assert!(resolved.allow.contains("gh"));
        assert!(resolved.allow.contains("kubectl"));
        assert!(!resolved.allow.contains("cp"));
        // User's pipeline preserved (project didn't specify pipeline)
        assert!(resolved.pipeline.contains("grep"));

        Ok(())
    }

    #[test]
    fn commands_absent_project_falls_through() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let user = dir.path().join("user.toml");
        fs::write(
            &user,
            r#"
[commands]
allow = ["git", "gh"]
"#,
        )?;

        let project = dir.path().join("project.toml");
        fs::write(&project, "")?;

        let config = Config::load_from_sources(&[user, project])?;
        let resolved = config
            .resolved_commands
            .expect("resolved_commands should be Some");
        assert!(resolved.allow.contains("git"));
        assert!(resolved.allow.contains("gh"));

        Ok(())
    }

    #[test]
    fn commands_client_enforcement_only() -> anyhow::Result<()> {
        let dir = tempdir()?;
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r"
[commands]
client_enforcement_only = true
",
        )?;

        let config = Config::load_from_sources(&[path])?;
        let resolved = config
            .resolved_commands
            .expect("resolved_commands should be Some");
        assert!(resolved.client_enforcement_only);
        assert!(!resolved.is_active());

        Ok(())
    }

    #[test]
    fn commands_client_enforcement_only_with_allow_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[commands]
client_enforcement_only = true
allow = ["git"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("client_enforcement_only"),
            "error should mention client_enforcement_only: {err}",
        );
    }

    #[test]
    fn commands_allow_pipeline_overlap_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[commands]
allow = ["grep", "git"]
pipeline = ["grep"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("grep") && err.contains("allow") && err.contains("pipeline"),
            "error should mention grep in both lists: {err}",
        );
    }

    #[test]
    fn commands_deny_not_in_allow_rejected() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[commands]
allow = ["git"]

[commands.deny]
sqlite3 = ["-cmd"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("sqlite3") && err.contains("not in `allow`"),
            "error should mention sqlite3 not in allow: {err}",
        );
    }

    #[test]
    fn commands_three_layer_merge() -> anyhow::Result<()> {
        let dir = tempdir()?;

        let user = dir.path().join("user.toml");
        fs::write(
            &user,
            r#"
[commands]
build = "make"
allow = ["git", "gh", "cp"]
pipeline = ["grep", "head"]

[commands.deny]
git = ["grep"]
"#,
        )?;

        let project = dir.path().join("project.toml");
        fs::write(
            &project,
            r#"
[commands]
allow = ["git", "gh", "kubectl"]

[commands.deny]
git = ["ls-files"]
"#,
        )?;

        let explicit = dir.path().join("explicit.toml");
        fs::write(
            &explicit,
            r#"
[commands]
build = "npm"
"#,
        )?;

        let config = Config::load_from_sources(&[user, project, explicit])?;
        let resolved = config
            .resolved_commands
            .expect("resolved_commands should be Some");
        // Project replaces user's allow
        assert!(resolved.allow.contains("git"));
        assert!(resolved.allow.contains("gh"));
        assert!(resolved.allow.contains("kubectl"));
        assert!(!resolved.allow.contains("cp"));
        // User's pipeline preserved
        assert!(resolved.pipeline.contains("grep"));
        assert!(resolved.pipeline.contains("head"));
        // Deny entries merged across layers
        let git_deny = resolved.deny.get("git").expect("git deny");
        assert!(git_deny.contains("grep"));
        assert!(git_deny.contains("ls-files"));
        // Explicit overrides build
        assert_eq!(resolved.default_build, vec!["npm"]);

        Ok(())
    }

    // ── Root marker defaults tests ───────────────────────────────────

    #[test]
    fn test_default_root_markers_from_defaults() -> anyhow::Result<()> {
        // Rust gets root_markers = ["Cargo.toml"] from defaults/languages.toml.
        // User config only adds server binding — markers come from defaults.
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )
        .expect("write config");

        let config = Config::load_from_sources(&[config_path])?;
        let rust = config.language.get("rust").expect("rust");
        assert_eq!(
            rust.root_markers.as_deref(),
            Some(&["Cargo.toml".to_string()][..]),
            "default root_markers should be applied for rust",
        );
        Ok(())
    }

    #[test]
    fn test_explicit_root_markers_override_defaults() -> anyhow::Result<()> {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
root_markers = ["rust-toolchain.toml"]
"#,
        )
        .expect("write config");

        let config = Config::load_from_sources(&[config_path])?;
        let rust = config.language.get("rust").expect("rust");
        assert_eq!(
            rust.root_markers.as_deref(),
            Some(&["rust-toolchain.toml".to_string()][..]),
            "user-specified root_markers should override defaults",
        );
        Ok(())
    }

    #[test]
    fn test_empty_root_markers_disables_defaults() -> anyhow::Result<()> {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
root_markers = []
"#,
        )
        .expect("write config");

        let config = Config::load_from_sources(&[config_path])?;
        let rust = config.language.get("rust").expect("rust");
        assert_eq!(
            rust.root_markers.as_deref(),
            Some(&[][..]),
            "explicit empty root_markers should disable defaults",
        );
        assert!(
            rust.active_markers().is_none(),
            "active_markers should return None for empty markers",
        );
        Ok(())
    }

    #[test]
    fn test_unknown_language_no_default_markers() -> anyhow::Result<()> {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");

        fs::write(
            &config_path,
            r#"
[server.my-custom-server]
command = "my-server"

[language.custom]
servers = ["my-custom-server"]
"#,
        )
        .expect("write config");

        let config = Config::load_from_sources(&[config_path])?;
        let custom = config.language.get("custom").expect("custom");
        assert!(
            custom.root_markers.is_none(),
            "unknown languages should not get default root_markers",
        );
        Ok(())
    }

    // ── Built-in server defaults ────────────────────────────────────

    #[test]
    fn test_builtin_server_resolves() -> anyhow::Result<()> {
        // servers = ["gopls"] with no user [server.gopls] should resolve
        // to the built-in definition.
        let config = Config::load_from_sources(&[])?;
        let go = config.language.get("go").expect("go language config");
        assert_eq!(go.servers, Some(vec![ServerBinding::new("gopls")]));
        let gopls = config.server.get("gopls").expect("gopls server def");
        assert_eq!(gopls.command, "gopls");
        Ok(())
    }

    #[test]
    fn test_user_server_overrides_builtin() -> anyhow::Result<()> {
        // User [server.rust-analyzer] with custom command completely
        // replaces the built-in.
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;
        let ra = config
            .server
            .get("rust-analyzer")
            .expect("rust-analyzer server def");
        assert_eq!(ra.command, "rust-analyzer", "user command should win");
        assert!(ra.args.is_empty(), "built-in args should NOT be inherited");
        Ok(())
    }

    #[test]
    fn test_builtin_no_merge() -> anyhow::Result<()> {
        // User defines [server.rust-analyzer] with only command — built-in
        // args are NOT inherited.
        let dir = tempdir()?;
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[server.rust-analyzer]
command = "rust-analyzer"

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let config = Config::load_from_sources(&[config_path])?;
        let ra = config
            .server
            .get("rust-analyzer")
            .expect("rust-analyzer server def");
        assert_eq!(ra.command, "rust-analyzer");
        assert!(
            ra.args.is_empty(),
            "user override with no args should not inherit built-in args",
        );
        Ok(())
    }

    #[test]
    fn test_unknown_server_errors() {
        // servers = ["nonexistent"] with no definition anywhere produces
        // a validation error.
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            r#"
[language.custom]
extensions = ["xyz"]
servers = ["nonexistent"]
"#,
        )
        .expect("write config");

        let result = Config::load_from_sources(&[config_path]);
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("nonexistent"),
            "error should mention undefined server: {err}",
        );
    }

    #[test]
    fn test_builtin_servers_all_have_command() -> anyhow::Result<()> {
        // Every built-in server def must have a non-empty command.
        let config = Config::load_from_sources(&[])?;
        for (name, def) in &config.server {
            assert!(
                !def.command.is_empty(),
                "built-in server '{name}' has an empty command",
            );
        }
        Ok(())
    }

    #[test]
    fn test_builtin_language_servers_resolve() -> anyhow::Result<()> {
        // Every default language with a servers list should have all
        // referenced servers available.
        let config = Config::load_from_sources(&[])?;
        for (lang, lang_config) in &config.language {
            for binding in lang_config.servers() {
                assert!(
                    config.server.contains_key(&binding.name),
                    "language '{lang}' references server '{}' which is not defined",
                    binding.name,
                );
            }
        }
        Ok(())
    }

    // ── default_diagnostics_per_page tests ──────────────────────────

    #[test]
    fn diagnostics_per_page_default_is_50() {
        assert_eq!(default_diagnostics_per_page(), 50);
    }

    #[test]
    fn diagnostics_severity_default_is_error() {
        let tc = ToolsConfig::default();
        assert_eq!(tc.diagnostics_severity, "error");
        assert_eq!(tc.dirty_severity(), crate::filter::SEVERITY_ERROR);
    }

    #[test]
    fn dirty_severity_parses_and_falls_back() {
        let mut tc = ToolsConfig {
            diagnostics_severity: "warning".to_string(),
            ..ToolsConfig::default()
        };
        assert_eq!(tc.dirty_severity(), crate::filter::SEVERITY_WARNING);
        // An unrecognized value falls back to error rather than disabling the gate.
        tc.diagnostics_severity = "bogus".to_string();
        assert_eq!(tc.dirty_severity(), crate::filter::SEVERITY_ERROR);
    }

    #[test]
    fn diagnostics_budget_clamps_zero_to_one() {
        let tc = ToolsConfig {
            diagnostics_per_page: 0,
            ..ToolsConfig::default()
        };
        assert_eq!(tc.diagnostics_budget(), 1);
        let tc = ToolsConfig {
            diagnostics_per_page: 25,
            ..ToolsConfig::default()
        };
        assert_eq!(tc.diagnostics_budget(), 25);
    }

    // ── clamp_budgets tests ─────────────────────────────────────────

    #[test]
    fn clamp_budgets_leaves_valid_values() {
        let mut tc = ToolsConfig {
            grep: GrepConfig { budget: 4000 },
            glob: GlobConfig {
                budget: 2000,
                ..GlobConfig::default()
            },
            diagnostics_per_page: 50,
            ..ToolsConfig::default()
        };
        tc.clamp_budgets();
        assert_eq!(tc.grep.budget, 4000);
        assert_eq!(tc.glob.budget, 2000);
        assert_eq!(tc.diagnostics_per_page, 50);
    }

    #[test]
    fn clamp_budgets_raises_below_minimum() {
        let mut tc = ToolsConfig {
            grep: GrepConfig { budget: 500 },
            glob: GlobConfig {
                budget: 100,
                ..GlobConfig::default()
            },
            diagnostics_per_page: 0,
            ..ToolsConfig::default()
        };
        tc.clamp_budgets();
        assert_eq!(tc.grep.budget, 2000, "grep budget should clamp to 2000");
        assert_eq!(tc.glob.budget, 1000, "glob budget should clamp to 1000");
        assert_eq!(tc.diagnostics_per_page, 1, "diagnostics should clamp to 1");
    }

    #[test]
    fn clamp_budgets_at_exact_minimum_is_noop() {
        let mut tc = ToolsConfig {
            grep: GrepConfig { budget: 2000 },
            glob: GlobConfig {
                budget: 1000,
                ..GlobConfig::default()
            },
            diagnostics_per_page: 1,
            ..ToolsConfig::default()
        };
        tc.clamp_budgets();
        assert_eq!(tc.grep.budget, 2000, "at minimum should stay");
        assert_eq!(tc.glob.budget, 1000, "at minimum should stay");
        assert_eq!(tc.diagnostics_per_page, 1, "at minimum should stay");
    }

    // ── DispatchMethod tests ────────────────────────────────────────

    #[test]
    fn dispatch_method_as_str_all_variants() {
        use crate::config::language::DispatchMethod;
        assert_eq!(
            DispatchMethod::References.as_str(),
            "textDocument/references"
        );
        assert_eq!(
            DispatchMethod::DocumentSymbol.as_str(),
            "textDocument/documentSymbol"
        );
        assert_eq!(DispatchMethod::Rename.as_str(), "textDocument/rename");
        assert_eq!(
            DispatchMethod::Implementation.as_str(),
            "textDocument/implementation"
        );
        assert_eq!(
            DispatchMethod::CallHierarchy.as_str(),
            "textDocument/prepareCallHierarchy"
        );
        assert_eq!(
            DispatchMethod::TypeHierarchy.as_str(),
            "textDocument/prepareTypeHierarchy"
        );
    }

    #[test]
    fn dispatch_method_deserialize_all_variants() -> anyhow::Result<()> {
        use crate::config::language::DispatchMethod;

        #[derive(serde::Deserialize)]
        struct Wrapper {
            method: DispatchMethod,
        }

        let methods = [
            ("textDocument/references", DispatchMethod::References),
            (
                "textDocument/documentSymbol",
                DispatchMethod::DocumentSymbol,
            ),
            ("textDocument/rename", DispatchMethod::Rename),
            (
                "textDocument/implementation",
                DispatchMethod::Implementation,
            ),
            (
                "textDocument/prepareCallHierarchy",
                DispatchMethod::CallHierarchy,
            ),
            (
                "textDocument/prepareTypeHierarchy",
                DispatchMethod::TypeHierarchy,
            ),
        ];

        for (input, expected) in methods {
            let toml_str = format!("method = \"{input}\"");
            let parsed: Wrapper = toml::from_str(&toml_str)?;
            assert_eq!(
                parsed.method.as_str(),
                expected.as_str(),
                "deserialize '{input}' should produce correct variant",
            );
        }
        Ok(())
    }

    // ── Shipped config.example.toml round-trip ──────────────────────

    /// The onboarding artifact `plugins/catenary/config.example.toml` — the
    /// file users are told to "Copy to ~/.config/catenary/config.toml" — must
    /// load cleanly through the real config loader (the same `deserialize_source`
    /// → merge → validate path `Config::load` uses). This guards against the
    /// example drifting back to the pre-split format (`[language.*]` with inline
    /// `command`/`args`, or the removed `inherit` field), which the migration
    /// guards in `deserialize_source` hard-reject (bug 27).
    #[test]
    fn shipped_config_example_loads() -> anyhow::Result<()> {
        let example = std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugins/catenary/config.example.toml"
        ));
        assert!(
            example.exists(),
            "shipped example missing at {}",
            example.display(),
        );

        // Full loader pipeline: parse + migration guards + merge + validate.
        // A pre-split example (inline `command` on [language.*], or `inherit`)
        // makes this `?` propagate the migration-guard `bail!` and fail the test.
        let config = Config::load_from_sources(&[example])?;

        // Spot-check the split format took effect: the rust-analyzer override
        // is a [server.*] definition with a command, and the markdown default
        // resolves to lattice (decision 015), not marksman.
        let ra = config
            .server
            .get("rust-analyzer")
            .expect("rust-analyzer server def");
        assert_eq!(ra.command, "rustup");

        let markdown = config.language.get("markdown").expect("markdown language");
        assert_eq!(
            markdown.servers,
            Some(vec![ServerBinding::new("lattice")]),
            "markdown should default to lattice (decision 015)",
        );

        Ok(())
    }
}
