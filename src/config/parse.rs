// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! TOML deserialization, file reading, source merging, and env var overrides.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::source::Source;

use crate::logging::reaper::ReapPolicy;

use super::commands::{self, CommandsConfig};
use super::{
    Config, DiagnosticPrecedence, IconConfig, LanguageConfig, LinterConfig, NotificationConfig,
    RootsConfig, ServerBinding, ServerDef, ToolsConfig, TuiConfig, default_log_retention_days,
};

/// Embedded default classification config (lowest-priority layer).
const DEFAULT_LANGUAGES: &str = include_str!("../../defaults/languages.toml");

/// Embedded default server definitions (lowest-priority layer).
///
/// Parsed separately from `DEFAULT_LANGUAGES` because
/// `deserialize_source` rejects `[server.*]`-only configs (migration
/// guard for old user configs). Built-in server defs are merged into
/// `config.server` before any user/project config, so user entries
/// with the same key completely replace the built-in default.
pub const DEFAULT_SERVERS: &str = include_str!("../../defaults/servers.toml");

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
    observability: Option<ReapPolicy>,

    #[serde(default)]
    roots: Option<RootsConfig>,

    #[serde(default)]
    linter: HashMap<String, LinterConfig>,

    /// Cross-feeder precedence policies (`[[diagnostic_precedence]]`).
    ///
    /// `None` = the layer did not mention the section (inherit the earlier
    /// layer, ultimately the seeded [`DiagnosticPrecedence::rust_analyzer_default`]).
    /// `Some` (even empty) = the layer sets the policy list, replacing it —
    /// `diagnostic_precedence = []` is the explicit "no precedence" opt-out.
    #[serde(default)]
    diagnostic_precedence: Option<Vec<DiagnosticPrecedence>>,

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
/// 1. Embedded default server definitions (`defaults/servers.toml`)
/// 2. Embedded default language config (`defaults/languages.toml`)
/// 3. User/project/explicit files (the `sources` parameter)
/// 4. Environment variable overrides
///
/// Validation is applied after merging.
pub fn load_from_sources(sources: &[PathBuf]) -> Result<Config> {
    let mut config = Config::default();

    // Load embedded default server definitions (lowest priority).
    let default_servers = parse_server_defaults(DEFAULT_SERVERS)
        .context("Failed to parse embedded default server config")?;
    for (key, value) in default_servers {
        config.server.insert(key, value);
    }

    // Load embedded default classification config (includes server bindings).
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

    // Compile root_markers globs after validation. Same guarantee —
    // validation already checked each glob pattern.
    for lang_config in config.language.values_mut() {
        lang_config
            .compile_markers()
            .context("root_markers compilation failed after validation (bug)")?;
    }

    // Compile linter routing globs after validation. Validation already checks
    // each pattern with LspGlob::new(), so this just populates compiled_patterns.
    for linter in config.linter.values_mut() {
        linter
            .compile_patterns()
            .context("linter patterns compilation failed after validation (bug)")?;
    }

    // Compile cross-feeder precedence code bands after validation (ticket 02).
    // The seeded default is already compiled; this populates any user-provided
    // `[[diagnostic_precedence]]` policies, whose patterns validation checked.
    for precedence in &mut config.diagnostic_precedence {
        precedence.compile().context(
            "diagnostic_precedence code_pattern compilation failed after validation (bug)",
        )?;
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

/// Parse a `[server.*]` TOML document into a map of server definitions.
///
/// Used for the embedded `defaults/servers.toml` which contains only
/// `[server.*]` entries. This bypasses `deserialize_source` which
/// rejects server-only configs as the old deprecated format.
fn parse_server_defaults(contents: &str) -> Result<HashMap<String, ServerDef>> {
    #[derive(Deserialize)]
    struct ServerOnly {
        #[serde(default)]
        server: HashMap<String, ServerDef>,
    }
    let parsed: ServerOnly = toml::from_str(contents).context("Failed to parse server TOML")?;
    Ok(parsed.server)
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
/// **Structured sections** (`notifications`, `icons`, `tui`, `tools`,
/// `observability`, `roots`): `Option<T>` on `Config`. `None` means the source
/// did not mention the section; `Some` means it was present (even if all values
/// match defaults). Merge only overwrites when the later source is `Some`, so an
/// earlier source's explicit setting survives an unrelated later source.
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
    if other.observability.is_some() {
        config.observability = other.observability;
    }
    if other.roots.is_some() {
        config.roots = other.roots;
    }
    for (key, value) in other.linter {
        config.linter.insert(key, value);
    }
    // Whole-list replace when the layer sets the section (`Some`, even empty),
    // mirroring `servers = []`: an explicit empty array clears the seeded
    // default; absence (`None`) inherits the earlier layer.
    if let Some(precedence) = other.diagnostic_precedence {
        config.diagnostic_precedence = precedence;
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
                        servers: Some(vec![ServerBinding::new(server_name)]),
                        ..LanguageConfig::default()
                    },
                ));
            }
        }
    }
    results
}

/// Top-level keys allowed in `.catenary.toml` project config files.
///
/// The three per-root feeder toggles (`disable_lsp` / `disable_lint` /
/// `disable_diag`) replace the removed `lsp`/`enabled` key (workstream 34
/// ticket 00). The unsupported-key warning skips all of these.
const PROJECT_CONFIG_ALLOWED_KEYS: &[&str] = &[
    "disable_lsp",
    "disable_lint",
    "disable_diag",
    "language",
    "server",
    "linter",
    "diagnostic_precedence",
    "commands",
];

/// Per-root project configuration from `.catenary.toml`.
///
/// Contains the three orthogonal feeder toggles, `[language.*]` and
/// `[server.*]` sections, and an optional `[commands]` section. A root with
/// any toggle set still contributes `[commands]` config (both `build` and
/// `allow`) — the toggles only suppress the matching diagnostic feeder or
/// surface, never command/build resolution.
#[derive(Debug, Clone, Default)]
pub struct ProjectConfig {
    /// Drops the LSP feeder for this root (default `false`).
    ///
    /// No language servers spawn, no grep/glob enrichment, no LSP
    /// diagnostics. The root stays tracked (`roots ls`, build/command
    /// resolution, linters, gate). Polarity flip of the removed
    /// `lsp = false`.
    pub disable_lsp: bool,
    /// Drops the linter feeder for this root (default `false`).
    ///
    /// No standalone-linter diagnostics. No-op until workstream 34 ticket
    /// 01 lands the linter framework — parsed and stored here so the toggle
    /// is stable and reflected by `catenary doctor`.
    pub disable_lint: bool,
    /// Suppresses the diagnostics surface for this root (default `false`).
    ///
    /// The editing→`catenary diagnostics` gate and its output are turned
    /// off, but LSP servers still run for grep/glob navigation. A surface
    /// suppressor, not a feeder toggle.
    pub disable_diag: bool,
    /// Language definitions from the project config.
    pub language: HashMap<String, LanguageConfig>,
    /// Server definitions from the project config.
    pub server: HashMap<String, ServerDef>,
    /// Standalone-linter definitions from the project config (`[linter.*]`).
    ///
    /// The per-root half of the linter feeder (workstream 34 ticket 01). Merged
    /// with the user `[linter.*]` (project wins on a name collision) to form the
    /// root's effective linter set.
    pub linter: HashMap<String, LinterConfig>,
    /// Cross-feeder precedence policies from the project config
    /// (`[[diagnostic_precedence]]`, workstream 34 ticket 02).
    ///
    /// Per-root override: when non-empty, this list replaces the user-level
    /// `diagnostic_precedence` for files under this root (see
    /// [`LspClientManager::effective_precedence`]). Empty = unspecified, so the
    /// user policy is inherited.
    ///
    /// [`LspClientManager::effective_precedence`]: crate::lsp::LspClientManager::effective_precedence
    pub diagnostic_precedence: Vec<DiagnosticPrecedence>,
    /// Command filter configuration from the project config.
    ///
    /// `build` is per-root (each root can have its own build tool).
    /// `allow` replaces the user's list for this root's contribution.
    /// `pipeline` and `deny` follow the same replacement semantics.
    pub commands: Option<CommandsConfig>,
}

/// Project `[commands]` keys that Catenary ignores — everything but `build`.
///
/// Detected on the **raw** TOML rather than the parsed [`CommandsConfig`] so a
/// boolean written as `= false` (e.g. `client_enforcement_only = false`, a
/// project asking for enforcement the daemon-global filter won't grant) is
/// caught as well as `= true` — the parsed form can't tell `false` from
/// absent. See [`commands::PROJECT_IGNORED_COMMAND_KEYS`] for the rationale.
fn ignored_project_command_keys(raw: &toml::Value) -> Vec<&'static str> {
    let Some(table) = raw.get("commands").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    commands::PROJECT_IGNORED_COMMAND_KEYS
        .iter()
        .copied()
        .filter(|key| table.contains_key(*key))
        .collect()
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

    // Hard rename at the 2.0 boundary: the `lsp` kill switch (and its
    // deprecated `enabled` alias) is gone, replaced by the three orthogonal
    // toggles below. Error rather than silently ignore — a stale `lsp = false`
    // means "disable LSP here", and dropping it would silently re-enable LSP.
    if raw.get("lsp").is_some() || raw.get("enabled").is_some() {
        bail!(
            "Project config {}: the `lsp`/`enabled` key was removed in 2.0 — \
             use `disable_lsp` instead (polarity flips: `lsp = false` becomes \
             `disable_lsp = true`).",
            config_path.display(),
        );
    }

    let disable_lsp = parse_bool_flag(&raw, "disable_lsp", &config_path)?;
    let disable_lint = parse_bool_flag(&raw, "disable_lint", &config_path)?;
    let disable_diag = parse_bool_flag(&raw, "disable_diag", &config_path)?;

    // Deserialize only the supported sections.
    let mut language: HashMap<String, LanguageConfig> = raw
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

    // Compile root_markers globs on project LanguageConfig entries.
    for (name, lang_config) in &mut language {
        lang_config.compile_markers().with_context(|| {
            format!(
                "Project config {}: [language.{name}] root_markers compilation failed",
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

    // Parse [linter.*] entries (workstream 34 ticket 01).
    let mut linter: HashMap<String, LinterConfig> = raw
        .get("linter")
        .map(|v| {
            toml::Value::try_into(v.clone()).with_context(|| {
                format!(
                    "Failed to parse [linter.*] in project config: {}",
                    config_path.display()
                )
            })
        })
        .transpose()?
        .unwrap_or_default();

    // Compile routing globs and validate. An entry with an empty `command` is
    // only valid as a `disable = true` override of a user-configured linter;
    // otherwise it is a malformed definition.
    for (name, linter_config) in &mut linter {
        if linter_config.command.is_empty()
            && !linter_config.disable
            && (!linter_config.args.is_empty() || !linter_config.patterns.is_empty())
        {
            bail!(
                "Project config {}: [linter.{name}] has an empty `command`",
                config_path.display()
            );
        }
        linter_config.compile_patterns().with_context(|| {
            format!(
                "Project config {}: [linter.{name}] patterns compilation failed",
                config_path.display()
            )
        })?;
    }

    // Parse [[diagnostic_precedence]] policies (workstream 34 ticket 02). A
    // non-empty list overrides the user-level precedence for this root; absence
    // (empty) inherits it.
    let mut diagnostic_precedence: Vec<DiagnosticPrecedence> = raw
        .get("diagnostic_precedence")
        .map(|v| {
            toml::Value::try_into(v.clone()).with_context(|| {
                format!(
                    "Failed to parse [[diagnostic_precedence]] in project config: {}",
                    config_path.display()
                )
            })
        })
        .transpose()?
        .unwrap_or_default();
    for precedence in &mut diagnostic_precedence {
        precedence.compile().with_context(|| {
            format!(
                "Project config {}: [[diagnostic_precedence]] code_pattern compilation failed",
                config_path.display()
            )
        })?;
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

    // Command enforcement is user-level only (ticket 15): a project
    // `.catenary.toml [commands]` honors `build` and nothing else. The filter
    // resolves daemon-globally, so honoring any other key here would change
    // the filter for every connected session. Warn loudly rather than dropping
    // them silently — detected on the raw TOML so an explicit `= false` on a
    // boolean (a project asking for *more* enforcement, the silent direction)
    // is caught too. The keys still parse and are ignored at merge time
    // (`merge_project_commands`).
    let ignored = ignored_project_command_keys(&raw);
    if !ignored.is_empty() {
        tracing::warn!(
            source = Source::ConfigValidation.as_str(),
            path = %config_path.display(),
            "Project config {}: [commands] keys other than `build` are ignored \
             at project scope ({}) — command enforcement is a daemon-wide, \
             user-level decision (one daemon serves every session). Move them \
             to your user config (~/.config/catenary/config.toml).",
            config_path.display(),
            ignored.join(", "),
        );
    }

    Ok(Some(ProjectConfig {
        disable_lsp,
        disable_lint,
        disable_diag,
        language,
        server,
        linter,
        diagnostic_precedence,
        commands: commands_config,
    }))
}

/// Parses a top-level boolean toggle, defaulting to `false` when absent.
///
/// Errors when the key is present but not a boolean — a malformed value
/// (e.g. `disable_lsp = "true"`) would otherwise silently default to `false`,
/// quietly defeating the user's intent to disable a feeder.
fn parse_bool_flag(raw: &toml::Value, key: &str, config_path: &std::path::Path) -> Result<bool> {
    match raw.get(key) {
        None => Ok(false),
        Some(toml::Value::Boolean(b)) => Ok(*b),
        Some(_) => bail!(
            "Project config {}: `{key}` must be a boolean (true or false).",
            config_path.display(),
        ),
    }
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
    fn ignored_project_command_keys_detects_presence_including_false_bools() {
        // Only `build` → nothing ignored.
        let raw: toml::Value =
            toml::from_str("[commands]\nbuild = \"make\"\n").expect("valid toml");
        assert!(ignored_project_command_keys(&raw).is_empty());

        // No `[commands]` table → nothing ignored.
        let raw: toml::Value = toml::from_str("lsp = true\n").expect("valid toml");
        assert!(ignored_project_command_keys(&raw).is_empty());

        // Explicit `= false` on the enforcement booleans is still flagged — the
        // silent, dangerous direction (a project asking for *more* enforcement
        // than the daemon-global, user-level filter grants).
        let raw: toml::Value = toml::from_str(
            "[commands]\nbuild = \"make\"\n\
             client_enforcement_only = false\nallow_file_redirects = false\n",
        )
        .expect("valid toml");
        let ignored = ignored_project_command_keys(&raw);
        assert!(ignored.contains(&"client_enforcement_only"));
        assert!(ignored.contains(&"allow_file_redirects"));
        assert!(!ignored.contains(&"build"));

        // Non-boolean enforcement keys and guidance are flagged by presence.
        let raw: toml::Value = toml::from_str(
            "[commands]\nallow = [\"git\"]\npipeline = [\"grep\"]\n\
             [commands.deny]\ngit = [\"push\"]\n\
             [commands.deny_flags]\ngit = [\"-f\"]\n\
             [commands.guidance.scan]\nmessage = \"x\"\ncommands = [\"rg\"]\n",
        )
        .expect("valid toml");
        let ignored = ignored_project_command_keys(&raw);
        for key in ["allow", "pipeline", "deny", "deny_flags", "guidance"] {
            assert!(ignored.contains(&key), "{key} should be flagged");
        }
    }

    #[test]
    fn load_project_config_tolerates_ignored_enforcement_keys() -> Result<()> {
        // A project [commands] with enforcement keys must still load (build is
        // honored; the rest warns and is ignored at merge time) — not bail.
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
[commands]
build = "make"
allow = ["git", "kubectl"]
"#,
        )?;
        let config = load_project_config(dir.path())?.expect("project config");
        let cmds = config.commands.expect("commands parsed");
        assert!(cmds.build.is_some(), "build is honored at project scope");
        assert_eq!(
            cmds.allow,
            Some(vec!["git".to_string(), "kubectl".to_string()]),
            "enforcement keys still parse (ignored later, at merge time)",
        );

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
    fn test_load_project_config_toggles_default_false() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            "\n[language.rust]\nservers = []\n",
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(!config.disable_lsp, "disable_lsp should default to false");
        assert!(!config.disable_lint, "disable_lint should default to false");
        assert!(!config.disable_diag, "disable_diag should default to false");

        Ok(())
    }

    #[test]
    fn test_load_project_config_disable_lsp() -> Result<()> {
        let dir = tempdir()?;
        fs::write(dir.path().join(".catenary.toml"), "disable_lsp = true\n")?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(config.disable_lsp, "disable_lsp should be true");
        assert!(!config.disable_lint);
        assert!(!config.disable_diag);

        Ok(())
    }

    #[test]
    fn test_load_project_config_disable_lint_and_diag() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            "disable_lint = true\ndisable_diag = true\n",
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(!config.disable_lsp);
        assert!(config.disable_lint, "disable_lint should be true");
        assert!(config.disable_diag, "disable_diag should be true");

        Ok(())
    }

    #[test]
    fn test_load_project_config_lsp_key_removed_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join(".catenary.toml"), "lsp = false\n").expect("write");

        let result = load_project_config(dir.path());
        let err = format!("{:#}", result.expect_err("removed `lsp` key should error"));
        assert!(
            err.contains("disable_lsp"),
            "error should point to disable_lsp: {err}",
        );
    }

    #[test]
    fn test_load_project_config_enabled_key_removed_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join(".catenary.toml"), "enabled = false\n").expect("write");

        let result = load_project_config(dir.path());
        let err = format!(
            "{:#}",
            result.expect_err("removed `enabled` key should error")
        );
        assert!(
            err.contains("disable_lsp"),
            "error should point to disable_lsp: {err}",
        );
    }

    #[test]
    fn test_load_project_config_non_bool_toggle_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(dir.path().join(".catenary.toml"), "disable_lsp = \"yes\"\n").expect("write");

        let result = load_project_config(dir.path());
        let err = format!("{:#}", result.expect_err("non-bool toggle should error"));
        assert!(
            err.contains("must be a boolean"),
            "error should mention boolean: {err}",
        );
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
        assert_eq!(
            cmds.build.as_ref().map(|b| &b.0[..]),
            Some(["npm".to_string()].as_slice()),
        );
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
        assert!(!config.disable_lsp, "toggles default to false");
        assert!(!config.disable_lint);
        assert!(!config.disable_diag);
        assert!(config.language.is_empty());
        assert!(config.server.is_empty());
        assert!(config.commands.is_some());
        assert_eq!(
            config
                .commands
                .expect("commands")
                .build
                .as_ref()
                .map(|b| &b.0[..]),
            Some(["make".to_string()].as_slice()),
        );

        Ok(())
    }

    #[test]
    fn test_load_project_config_disabled_with_commands() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            r#"
disable_lsp = true

[commands]
build = "make"
allow = ["git"]
"#,
        )?;

        let result = load_project_config(dir.path())?;
        let config = result.expect("should find project config");
        assert!(config.disable_lsp);
        let cmds = config.commands.expect("commands present despite disabled");
        assert_eq!(
            cmds.build.as_ref().map(|b| &b.0[..]),
            Some(["make".to_string()].as_slice()),
        );
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

    // ── Project config [linter.*] (ticket 01) ───────────────────────

    #[test]
    fn test_load_project_config_linter() -> Result<()> {
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            "[linter.shellcheck]\ncommand = \"shellcheck\"\n\
             args = [\"-f\", \"json1\"]\npatterns = [\"**/*.sh\"]\n",
        )?;

        let config = load_project_config(dir.path())?.expect("project config");
        let sc = config.linter.get("shellcheck").expect("shellcheck linter");
        assert_eq!(sc.command, "shellcheck");
        assert_eq!(sc.args, vec!["-f".to_string(), "json1".to_string()]);
        assert_eq!(sc.patterns, vec!["**/*.sh".to_string()]);
        // Routing globs are compiled at load.
        assert!(sc.matches(std::path::Path::new("scripts/x.sh")));

        Ok(())
    }

    #[test]
    fn test_load_project_config_linter_disable_override_no_command() -> Result<()> {
        // A project entry can disable a user-configured linter by name with no
        // command of its own — that is a valid override, not a malformed entry.
        let dir = tempdir()?;
        fs::write(
            dir.path().join(".catenary.toml"),
            "[linter.shellcheck]\ndisable = true\n",
        )?;

        let config = load_project_config(dir.path())?.expect("project config");
        let sc = config.linter.get("shellcheck").expect("shellcheck linter");
        assert!(sc.disable);
        assert!(sc.command.is_empty());

        Ok(())
    }

    #[test]
    fn test_load_project_config_linter_empty_command_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".catenary.toml"),
            "[linter.shellcheck]\npatterns = [\"**/*.sh\"]\n",
        )
        .expect("write");

        let result = load_project_config(dir.path());
        let err = format!("{:#}", result.expect_err("empty command should error"));
        assert!(err.contains("empty `command`"), "got: {err}");
    }

    #[test]
    fn test_load_project_config_linter_invalid_glob_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".catenary.toml"),
            "[linter.x]\ncommand = \"x\"\npatterns = [\"[bad\"]\n",
        )
        .expect("write");

        let result = load_project_config(dir.path());
        assert!(
            result.is_err(),
            "invalid glob must fail project-config load"
        );
    }

    // ── Project config [[diagnostic_precedence]] (ticket 02) ─────────

    #[test]
    fn test_load_project_config_precedence() -> Result<()> {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".catenary.toml"),
            "[[diagnostic_precedence]]\n\
             advisory_sources = [\"adv\"]\n\
             authoritative_sources = [\"auth\"]\n\
             code_pattern = \"^E[0-9]+$\"\n",
        )
        .expect("write");

        let config = load_project_config(dir.path())?.expect("project config");
        assert_eq!(config.diagnostic_precedence.len(), 1);
        let policy = &config.diagnostic_precedence[0];
        assert!(policy.is_advisory("adv"));
        assert!(policy.is_authoritative("auth"));
        // Compiled at load — the band matches an E#### code.
        assert!(policy.code_in_band("E0107"));
        assert!(!policy.code_in_band("other"));
        Ok(())
    }

    #[test]
    fn test_load_project_config_precedence_invalid_regex_errors() {
        let dir = tempdir().expect("tempdir");
        fs::write(
            dir.path().join(".catenary.toml"),
            "[[diagnostic_precedence]]\ncode_pattern = \"^E[0-9+$\"\n",
        )
        .expect("write");

        let result = load_project_config(dir.path());
        assert!(
            result.is_err(),
            "invalid precedence regex must fail project-config load"
        );
    }
}
