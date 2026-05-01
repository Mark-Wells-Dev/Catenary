// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! TOML deserialization, file reading, source merging, and env var overrides.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::source::Source;

use super::commands::{self, CommandsConfig};
use super::{
    Config, IconConfig, LanguageConfig, NotificationConfig, ServerBinding, ServerDef, ToolsConfig,
    TuiConfig, default_log_retention_days,
};

/// Embedded default classification config (lowest-priority layer).
const DEFAULT_LANGUAGES: &str = include_str!("../../defaults/languages.toml");

/// TOML deserialization target for a single config source.
///
/// Each TOML file is deserialized into this struct. The `commands` field
/// is validated per-layer and folded into `Config::resolved_commands`
/// during merge, then discarded — it never appears on the final `Config`.
#[derive(Debug, Deserialize, Clone)]
struct RawConfig {
    #[serde(default = "default_log_retention_days")]
    log_retention_days: i64,

    #[serde(default)]
    language: HashMap<String, LanguageConfig>,

    #[serde(default)]
    server: HashMap<String, ServerDef>,

    #[serde(default)]
    notifications: Option<NotificationConfig>,

    #[serde(default)]
    icons: Option<IconConfig>,

    #[serde(default)]
    tui: Option<TuiConfig>,

    #[serde(default)]
    tools: Option<ToolsConfig>,

    #[serde(default)]
    commands: Option<CommandsConfig>,
}

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
pub fn load() -> Result<Config> {
    let sources = config_sources();
    load_from_sources(&sources)
}

/// Discover configuration file paths in standard order.
///
/// Returns the list of paths that would be loaded (later overrides earlier):
/// 1. User config (`~/.config/catenary/config.toml`)
/// 2. Explicit file from `CATENARY_CONFIG` env var
///
/// Project-local config (`.catenary.toml`) is not included — it is loaded
/// per-root by [`load_project_config`] and stored on `LspClientManager`.
#[must_use]
pub fn config_sources() -> Vec<PathBuf> {
    let mut sources: Vec<PathBuf> = Vec::new();

    // 1. User config directory (~/.config/catenary/config.toml)
    if let Some(config_dir) = dirs::config_dir() {
        let config_path = config_dir.join("catenary").join("config.toml");
        if config_path.exists() {
            sources.push(config_path);
        }
    }

    // 2. Explicit file from CATENARY_CONFIG env var
    if let Ok(path) = std::env::var("CATENARY_CONFIG") {
        sources.push(PathBuf::from(path));
    }

    sources
}

/// Load configuration from an explicit list of file paths.
///
/// Sources are merged in order (later overrides earlier):
/// 1. Embedded default config (`defaults/languages.toml`)
/// 2. User/project/explicit files (the `sources` parameter)
/// 3. Environment variable overrides
///
/// Validation is applied after merging.
pub fn load_from_sources(sources: &[PathBuf]) -> Result<Config> {
    let mut config = Config::default();

    // Load embedded default classification config (lowest priority).
    let defaults =
        deserialize_source(DEFAULT_LANGUAGES).context("Failed to parse embedded default config")?;
    merge(&mut config, defaults);

    for source in sources {
        let contents = std::fs::read_to_string(source)
            .with_context(|| format!("Failed to read config file: {}", source.display()))?;
        let layer = deserialize_source(&contents)
            .with_context(|| format!("Failed to parse config file: {}", source.display()))?;

        // Validate commands config per-layer (before merging destroys the raw form).
        if let Some(ref cmds) = layer.commands {
            let (errors, warnings) = commands::validate(cmds);
            if !errors.is_empty() {
                bail!(
                    "Configuration errors in {}:\n{}",
                    source.display(),
                    errors.join("\n"),
                );
            }
            for warning in warnings {
                tracing::warn!(source = %source.display(), "{warning}");
            }
        }

        merge(&mut config, layer);
    }

    config.apply_env_overrides();

    if let Some(ref mut tools) = config.tools {
        tools.clamp_budgets();
    }

    let errors = config.validate();
    if !errors.is_empty() {
        bail!("Configuration errors:\n{}", errors.join("\n"));
    }

    // Compile file_patterns globs after validation. Validation already
    // checks each pattern with LspGlob::new(), so this is guaranteed to
    // succeed — it just populates the compiled_patterns field.
    for server_def in config.server.values_mut() {
        server_def
            .compile_patterns()
            .context("file_patterns compilation failed after validation (bug)")?;
    }

    Ok(config)
}

/// Keys that belong on [`ServerDef`](super::ServerDef), not `LanguageConfig`.
///
/// Must match every config-visible field on `ServerDef` (i.e. every
/// field except `#[serde(skip)]`). `test_server_def_keys_sync` enforces this.
pub const SERVER_DEF_KEYS: &[&str] = &[
    "command",
    "args",
    "initialization_options",
    "settings",
    "min_severity",
    "env",
    "file_patterns",
    "single_file",
];

/// Deserialize a TOML source, handling the `[server.*]` / `[language.*]`
/// disambiguation.
///
/// Three cases:
/// - `[server.*]` with `command` fields and NO `[language.*]` → old deprecated
///   format. **Hard error** directing the user to `catenary doctor`.
/// - Both `[server.*]` and `[language.*]` → new format. `[server.*]` entries
///   are parsed as `ServerDef`.
/// - Only `[language.*]` (or neither) → intermediate/new format, parsed directly.
///
/// Additionally, `[language.*]` entries containing inline server definition
/// fields (`command`, `args`, `initialization_options`, `settings`) are
/// rejected with a migration message — these fields now live in `[server.*]`.
fn deserialize_source(contents: &str) -> Result<RawConfig> {
    let raw: toml::Value = toml::from_str(contents).context("Failed to parse TOML")?;

    let has_server = raw.get("server").is_some();
    let has_language = raw.get("language").is_some();

    if has_server && !has_language {
        // Old deprecated format: [server.*] used as language-keyed entries.
        // Check if any entry has a `command` field (distinguishes old format
        // from an accidental empty [server.*] table).
        let is_old_format = raw
            .get("server")
            .and_then(toml::Value::as_table)
            .is_some_and(|t| {
                t.values().any(|v| {
                    v.as_table()
                        .is_some_and(|entry| entry.contains_key("command"))
                })
            });

        if is_old_format {
            bail!(
                "Config uses deprecated [server.*] key for language definitions — \
                 rename [server.*] entries to [language.*] and define servers \
                 in [server.*] with the new format. Run `catenary doctor` for guidance."
            );
        }
    }

    // Reject [language.*] entries that contain inline server definition fields.
    // These fields now belong in [server.*].
    if let Some(lang_table) = raw.get("language").and_then(toml::Value::as_table) {
        for (lang_key, entry) in lang_table {
            if let Some(entry_table) = entry.as_table() {
                if entry_table.contains_key("inherit") {
                    bail!(
                        "[language.{lang_key}] uses the removed `inherit` field — \
                         copy the base language's `servers` list into \
                         [language.{lang_key}] instead. Run `catenary doctor` for guidance.",
                    );
                }

                let stale: Vec<&str> = SERVER_DEF_KEYS
                    .iter()
                    .copied()
                    .filter(|k| entry_table.contains_key(*k))
                    .collect();
                if !stale.is_empty() {
                    bail!(
                        "[language.{lang_key}] contains server definition fields ({}) — \
                         these now belong in [server.*]. Move them to a [server.*] \
                         entry and reference it via `servers = [\"...\"]` in \
                         [language.{lang_key}]. Run `catenary doctor` for guidance.",
                        stale.join(", "),
                    );
                }
            }
        }
    }

    // Detect old-format [commands] keys from workstream 14 (denylist model).
    // Instead of crashing, strip the section and warn — the notification
    // system surfaces this to the user once the server is running.
    let stripped_commands = has_old_commands_format(&raw);
    if stripped_commands {
        tracing::warn!(
            source = Source::ConfigParse.as_str(),
            "[commands] uses the old denylist format (deny_when_first or string-valued \
             deny entries). Catenary now uses an allowlist model — run `catenary config` \
             for the recommended template. Command filtering is disabled until the \
             config is updated.",
        );
    }

    // When old-format [commands] was detected, strip it from the Value and
    // deserialize from that (try_into). Otherwise deserialize from the
    // original string to preserve source positions in error messages.
    let config: RawConfig = if stripped_commands {
        let mut raw = raw;
        if let Some(table) = raw.as_table_mut() {
            table.remove("commands");
        }
        raw.try_into()
            .context("Failed to deserialize configuration")?
    } else {
        toml::from_str(contents).context("Failed to deserialize configuration")?
    };

    Ok(config)
}

/// Check whether a raw TOML value contains old-format `[commands]` keys.
///
/// Detects `deny_when_first` (silently ignored by serde) and string-valued
/// `deny` entries (would cause a type error). Used by both `deserialize_source`
/// and `load_project_config` to warn-and-strip instead of crashing.
fn has_old_commands_format(raw: &toml::Value) -> bool {
    let Some(cmd_table) = raw.get("commands").and_then(toml::Value::as_table) else {
        return false;
    };
    cmd_table.contains_key("deny_when_first")
        || cmd_table
            .get("deny")
            .and_then(toml::Value::as_table)
            .is_some_and(|d| d.values().any(toml::Value::is_str))
}

/// Merge a raw config layer into the resolved config. Later values override.
///
/// # Merge strategies
///
/// **Scalars** (`log_retention_days`): override only when the later
/// source differs from the default. Cannot distinguish "user explicitly
/// set the default" from "absent", but acceptable for simple numeric knobs.
///
/// **Maps** (`language`, `server`): key-level merge. Later source wins
/// per-key; keys absent from the later source are preserved.
///
/// **Structured sections** (`notifications`, `icons`, `tui`, `tools`):
/// `Option<T>` on `Config`. `None` means the source did not mention the
/// section; `Some` means it was present (even if all values match defaults).
/// Merge only overwrites when the later source is `Some`, so an earlier
/// source's explicit setting survives an unrelated later source.
///
/// **Commands** (`commands`): layered merge via `ResolvedCommands::merge`.
/// `allow` and `pipeline` replace; `deny` entries merge per-command;
/// `build` overwrites; `client_enforcement_only` is sticky. The raw
/// `CommandsConfig` is consumed and not stored on `Config`.
fn merge(config: &mut Config, other: RawConfig) {
    if other.log_retention_days != default_log_retention_days() {
        config.log_retention_days = other.log_retention_days;
    }
    for (key, value) in other.language {
        if let Some(existing) = config.language.get_mut(&key) {
            existing.merge(value);
        } else {
            config.language.insert(key, value);
        }
    }
    for (key, value) in other.server {
        config.server.insert(key, value);
    }
    if other.notifications.is_some() {
        config.notifications = other.notifications;
    }
    if other.icons.is_some() {
        config.icons = other.icons;
    }
    if other.tui.is_some() {
        config.tui = other.tui;
    }
    if other.tools.is_some() {
        config.tools = other.tools;
    }
    if let Some(ref cmds) = other.commands {
        config
            .resolved_commands
            .get_or_insert_with(super::ResolvedCommands::default)
            .merge(cmds);
    }
}

/// Apply environment variable overrides for supported keys.
pub(super) fn apply_env_overrides(config: &mut Config) {
    if let Ok(val) = std::env::var("CATENARY_LOG_RETENTION_DAYS")
        && let Ok(v) = val.parse()
    {
        config.log_retention_days = v;
    }

    // CATENARY_SERVERS: semicolon-separated "lang:command args" specs
    if let Ok(val) = std::env::var("CATENARY_SERVERS") {
        for (lang, server_def, lang_config) in parse_server_specs(&val) {
            config.server.insert(lang.clone(), server_def);
            config.language.insert(lang, lang_config);
        }
    }
}

/// Parse a `CATENARY_SERVERS` value into `(lang, ServerDef, LanguageConfig)` triples.
///
/// Format: semicolon-separated `"lang:command args"` specs. The language
/// key doubles as the server name for env-derived entries.
pub(super) fn parse_server_specs(val: &str) -> Vec<(String, ServerDef, LanguageConfig)> {
    let mut results = Vec::new();
    for spec in val.split(';') {
        let spec = spec.trim();
        if spec.is_empty() {
            continue;
        }
        if let Some((lang, command_str)) = spec.split_once(':') {
            let lang = lang.trim();
            let command_str = command_str.trim();
            let mut parts = command_str.split_whitespace();
            if let Some(program) = parts.next() {
                let cmd_args: Vec<String> = parts.map(std::string::ToString::to_string).collect();
                let server_name = lang.to_string();
                results.push((
                    lang.to_string(),
                    ServerDef {
                        command: program.to_string(),
                        args: cmd_args,
                        ..ServerDef::default()
                    },
                    LanguageConfig {
                        servers: vec![ServerBinding::new(server_name)],
                        ..LanguageConfig::default()
                    },
                ));
            }
        }
    }
    results
}

/// Top-level keys allowed in `.catenary.toml` project config files.
const PROJECT_CONFIG_ALLOWED_KEYS: &[&str] = &["enabled", "language", "server", "commands"];

/// Per-root project configuration from `.catenary.toml`.
///
/// Contains `enabled` (session kill switch), `[language.*]` and
/// `[server.*]` sections, and an optional `[commands]` section.
/// Disabled roots (`enabled = false`) still contribute `[commands]`
/// config (both `build` and `allow`) but not LSP config.
#[derive(Debug, Clone)]
pub struct ProjectConfig {
    /// Whether Catenary is enabled for this workspace (default `true`).
    ///
    /// When `false` on the primary root, the entire session is disabled:
    /// no tools, no servers, no hooks, no database writes.
    pub enabled: bool,
    /// Language definitions from the project config.
    pub language: HashMap<String, LanguageConfig>,
    /// Server definitions from the project config.
    pub server: HashMap<String, ServerDef>,
    /// Command filter configuration from the project config.
    ///
    /// `build` is per-root (each root can have its own build tool).
    /// `allow` replaces the user's list for this root's contribution.
    /// `pipeline` and `deny` follow the same replacement semantics.
    pub commands: Option<CommandsConfig>,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            language: HashMap::new(),
            server: HashMap::new(),
            commands: None,
        }
    }
}

/// Discovers and loads `.catenary.toml` at a workspace root.
///
/// Returns `None` if no `.catenary.toml` exists at the root.
/// Returns `Err` if the file exists but cannot be read or parsed.
///
/// The returned config is the raw project layer — not merged with
/// user config. Callers merge as needed via [`super::merge::deep_merge`].
///
/// # Errors
///
/// Returns an error if:
/// - The file exists but cannot be read.
/// - The file contains invalid TOML.
/// - A `[language.*]` entry uses the removed `inherit` field.
/// - A `[language.*]` entry contains inline server definition fields.
#[allow(clippy::too_many_lines, reason = "sequential validation steps")]
pub fn load_project_config(root: &std::path::Path) -> Result<Option<ProjectConfig>> {
    let config_path = root.join(".catenary.toml");
    if !config_path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(&config_path)
        .with_context(|| format!("Failed to read project config: {}", config_path.display()))?;

    let raw: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("Failed to parse project config: {}", config_path.display()))?;

    // Warn on unsupported top-level keys.
    if let Some(table) = raw.as_table() {
        for key in table.keys() {
            if !PROJECT_CONFIG_ALLOWED_KEYS.contains(&key.as_str()) {
                tracing::warn!(
                    source = Source::ConfigValidation.as_str(),
                    path = %config_path.display(),
                    key = key.as_str(),
                    "Project config {}: unsupported section [{}] — \
                     only [language.*], [server.*], and [commands] are \
                     allowed in .catenary.toml. Move [{key}] to your \
                     user config (~/.config/catenary/config.toml).",
                    config_path.display(),
                    key,
                );
            }
        }
    }

    // Detect old-format [commands] keys (denylist model from workstream 14).
    // Warn and strip instead of crashing — the notification system surfaces
    // this to the user. The project's command config is ignored.
    let mut raw = raw;
    if has_old_commands_format(&raw) {
        tracing::warn!(
            source = Source::ConfigParse.as_str(),
            path = %config_path.display(),
            "Project config {}: [commands] uses the old denylist format. \
             Catenary now uses an allowlist model — run `catenary config` \
             for the recommended template. This project's command config \
             is ignored.",
            config_path.display(),
        );
        if let Some(table) = raw.as_table_mut() {
            table.remove("commands");
        }
    }

    // Validate [language.*] entries for rejected fields, same as user config.
    if let Some(lang_table) = raw.get("language").and_then(toml::Value::as_table) {
        for (lang_key, entry) in lang_table {
            if let Some(entry_table) = entry.as_table() {
                if entry_table.contains_key("inherit") {
                    bail!(
                        "Project config {}: [language.{lang_key}] uses the removed \
                         `inherit` field — copy the base language's `servers` list \
                         into [language.{lang_key}] instead.",
                        config_path.display(),
                    );
                }

                let stale: Vec<&str> = SERVER_DEF_KEYS
                    .iter()
                    .copied()
                    .filter(|k| entry_table.contains_key(*k))
                    .collect();
                if !stale.is_empty() {
                    bail!(
                        "Project config {}: [language.{lang_key}] contains server \
                         definition fields ({}) — these belong in [server.*].",
                        config_path.display(),
                        stale.join(", "),
                    );
                }
            }
        }
    }

    // Parse the `enabled` flag (default true).
    let enabled = raw
        .get("enabled")
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);

    // Deserialize only the supported sections.
    let language: HashMap<String, LanguageConfig> = raw
        .get("language")
        .map(|v| {
            toml::Value::try_into(v.clone()).with_context(|| {
                format!(
                    "Failed to parse [language.*] in project config: {}",
                    config_path.display()
                )
            })
        })
        .transpose()?
        .unwrap_or_default();

    let mut server: HashMap<String, ServerDef> = raw
        .get("server")
        .map(|v| {
            toml::Value::try_into(v.clone()).with_context(|| {
                format!(
                    "Failed to parse [server.*] in project config: {}",
                    config_path.display()
                )
            })
        })
        .transpose()?
        .unwrap_or_default();

    // Compile file_patterns on project ServerDef entries.
    for (name, server_def) in &mut server {
        server_def.compile_patterns().with_context(|| {
            format!(
                "Project config {}: [server.{name}] file_patterns compilation failed",
                config_path.display()
            )
        })?;
    }

    // Validate server definitions — no empty commands.
    for (name, server_def) in &server {
        if server_def.command.is_empty()
            && (!server_def.args.is_empty()
                || server_def.initialization_options.is_some()
                || server_def.min_severity.is_some()
                || !server_def.file_patterns.is_empty())
        {
            bail!(
                "Project config {}: [server.{name}] has an empty `command`",
                config_path.display()
            );
        }
    }

    // Parse and validate [commands] section.
    let commands_config: Option<CommandsConfig> = raw
        .get("commands")
        .map(|v| {
            toml::Value::try_into(v.clone()).with_context(|| {
                format!(
                    "Failed to parse [commands] in project config: {}",
                    config_path.display()
                )
            })
        })
        .transpose()?;

    if let Some(ref cmds) = commands_config {
        let (errors, warnings) = commands::validate(cmds);
        if !errors.is_empty() {
            bail!(
                "Project config {} [commands] errors:\n{}",
                config_path.display(),
                errors.join("\n"),
            );
        }
        for warning in warnings {
            tracing::warn!(
                source = Source::ConfigValidation.as_str(),
                path = %config_path.display(),
                "{warning}",
            );
        }
    }

    Ok(Some(ProjectConfig {
        enabled,
        language,
        server,
        commands: commands_config,
    }))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod project_config_tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_load_project_config_found() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[server.rust-analyzer]
command = "rust-analyzer"
settings = { checkOnSave = true }

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(config.language.contains_key("rust"));
        assert!(config.server.contains_key("rust-analyzer"));
        let ra = &config.server["rust-analyzer"];
        assert_eq!(ra.command, "rust-analyzer");
        assert!(ra.settings.is_some());

        Ok(())
    }

    #[test]
    fn test_load_project_config_missing() -> Result<()> {
        let dir = tempdir()?;
        let result = load_project_config(dir.path())?;
        assert!(result.is_none());

        Ok(())
    }

    #[test]
    fn test_load_project_config_parse_error() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join(".catenary.toml"), "{{invalid toml").expect("write");

        let result = load_project_config(dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_load_project_config_rejects_inherit() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[language.typescriptreact]
inherit = "typescript"
"#,
        )
        .expect("write");

        let result = load_project_config(dir.path());
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("inherit"),
            "error should mention inherit: {err}",
        );
    }

    #[test]
    fn test_load_project_config_rejects_inline_server_keys() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[language.rust]
command = "rust-analyzer"
"#,
        )
        .expect("write");

        let result = load_project_config(dir.path());
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("command") && err.contains("[server.*]"),
            "error should mention server definition migration: {err}",
        );
    }

    #[test]
    fn test_load_project_config_warns_unsupported_sections() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r"
[tui]
auto_add_sessions = false

[language.rust]
servers = []
",
        )?;

        // Should succeed (warnings only, not errors) but the unsupported
        // sections are warned about. We verify it loads without error.
        let result = load_project_config(dir.path())?;
        assert!(result.is_some());

        Ok(())
    }

    #[test]
    fn test_load_project_config_language_and_server_only() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[server.pyright]
command = "pyright"
settings = { python = { analysis = { typeCheckingMode = "strict" } } }

[language.python]
servers = ["pyright"]
"#,
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should load cleanly");
        assert_eq!(config.language.len(), 1);
        assert_eq!(config.server.len(), 1);

        Ok(())
    }

    #[test]
    fn test_load_project_config_enabled_true_default() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            "\n[language.rust]\nservers = []\n",
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(config.enabled, "enabled should default to true");

        Ok(())
    }

    #[test]
    fn test_load_project_config_enabled_false() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join(".catenary.toml"), "enabled = false\n")?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(!config.enabled, "enabled should be false");

        Ok(())
    }

    #[test]
    fn test_load_project_config_enabled_true_explicit() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            "enabled = true\n\n[language.rust]\nservers = []\n",
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(config.enabled);

        Ok(())
    }

    #[test]
    fn test_config_sources_no_cwd_walk() {
        // config_sources() should not include .catenary.toml from cwd ancestors.
        let sources = config_sources();
        for source in &sources {
            assert!(
                source.file_name().and_then(|f| f.to_str()) != Some(".catenary.toml"),
                "config_sources() should not include .catenary.toml: {}",
                source.display(),
            );
        }
    }

    // ── Project config [commands] tests ─────────────────────────────

    #[test]
    fn test_load_project_config_with_commands() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[commands]
build = "npm"
allow = ["git", "gh"]

[commands.deny]
git = ["grep"]
"#,
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        let cmds = config.commands.expect("commands should be present");
        assert_eq!(cmds.build.as_deref(), Some("npm"));
        assert_eq!(cmds.allow.as_ref().expect("allow").len(), 2);
        let deny = cmds.deny.as_ref().expect("deny");
        assert!(
            deny.get("git")
                .expect("git deny")
                .contains(&"grep".to_string())
        );

        Ok(())
    }

    #[test]
    fn test_load_project_config_commands_only() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[commands]
build = "make"
"#,
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(config.enabled, "enabled defaults to true");
        assert!(config.language.is_empty());
        assert!(config.server.is_empty());
        assert!(config.commands.is_some());
        assert_eq!(
            config.commands.expect("commands").build.as_deref(),
            Some("make"),
        );

        Ok(())
    }

    #[test]
    fn test_load_project_config_disabled_with_commands() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
enabled = false

[commands]
build = "make"
allow = ["git"]
"#,
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(!config.enabled);
        let cmds = config.commands.expect("commands present despite disabled");
        assert_eq!(cmds.build.as_deref(), Some("make"));
        assert!(
            cmds.allow
                .as_ref()
                .expect("allow")
                .contains(&"git".to_string())
        );

        Ok(())
    }

    #[test]
    fn test_load_project_config_commands_validation_error() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[commands]
client_enforcement_only = true
allow = ["git"]
"#,
        )
        .expect("write");

        let result = load_project_config(dir.path());
        assert!(result.is_err());
        let err = format!("{:#}", result.expect_err("should error"));
        assert!(
            err.contains("client_enforcement_only"),
            "error should mention client_enforcement_only: {err}",
        );
    }

    #[test]
    fn test_load_project_config_no_commands() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            "\n[language.rust]\nservers = []\n",
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(config.commands.is_none());

        Ok(())
    }

    // ── Old-format [commands] stripped with warning ────────────────

    #[test]
    fn test_load_project_config_strips_deny_when_first() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[commands]
allow = ["git"]

[commands.deny_when_first]
cargo = "Use make instead"
"#,
        )?;

        // Should succeed — old format is stripped, not rejected.
        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        // [commands] section stripped entirely — treated as not configured.
        assert!(
            config.commands.is_none(),
            "old-format commands should be stripped",
        );

        Ok(())
    }

    #[test]
    fn test_load_project_config_strips_old_deny_strings() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[commands]
allow = ["git"]

[commands.deny]
cargo = "Use make instead"
"#,
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(
            config.commands.is_none(),
            "old-format commands should be stripped",
        );

        Ok(())
    }

    // ── Old-format [commands] stripped (user config) ─────────────────

    #[test]
    fn test_deserialize_source_strips_deny_when_first() -> Result<()> {
        let config = deserialize_source(
            r#"
[commands]
allow = ["git"]

[commands.deny_when_first]
cargo = "Use make instead"
"#,
        )?;
        // [commands] section stripped — parsed as absent.
        assert!(
            config.commands.is_none(),
            "old-format commands should be stripped",
        );

        Ok(())
    }

    #[test]
    fn test_deserialize_source_strips_old_deny_strings() -> Result<()> {
        let config = deserialize_source(
            r#"
[commands]
allow = ["git"]

[commands.deny]
cargo = "Use make instead"
"#,
        )?;
        assert!(
            config.commands.is_none(),
            "old-format commands should be stripped",
        );

        Ok(())
    }

    #[test]
    fn test_load_project_config_server_env() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[server.rust-analyzer]
command = "rust-analyzer"
env = { CLIPPY_DISABLE_DOCS_LINKS = "1", RUST_LOG = "info" }

[language.rust]
servers = ["rust-analyzer"]
"#,
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        let ra = &config.server["rust-analyzer"];
        let env = ra.env.as_ref().expect("env should be present");
        assert_eq!(
            env.get("CLIPPY_DISABLE_DOCS_LINKS").map(String::as_str),
            Some("1")
        );
        assert_eq!(env.get("RUST_LOG").map(String::as_str), Some("info"));

        Ok(())
    }

    #[test]
    fn test_load_project_config_server_env_absent() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[server.pyright]
command = "pyright-langserver"
args = ["--stdio"]

[language.python]
servers = ["pyright"]
"#,
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        let pyright = &config.server["pyright"];
        assert!(pyright.env.is_none(), "env should default to None");

        Ok(())
    }
}
