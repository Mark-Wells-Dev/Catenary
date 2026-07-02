// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Allowlist-based command filter configuration.
//!
//! `[commands]` defines which shell commands an agent may run. Three states:
//!
//! 1. **Absent** — no `[commands]` section. Not configured yet; emit a hint
//!    notification once per session at startup.
//! 2. **`client_enforcement_only = true`** — deliberate opt-out. No hint,
//!    no enforcement.
//! 3. **`allow = [...]` present** — active allowlist. Everything not
//!    explicitly allowed is denied.
//!
//! Keys:
//! - `client_enforcement_only` — deliberate opt-out flag.
//! - `build` — the project's build tool (e.g., `"make"`).
//! - `allow` — commands the agent can run unconditionally.
//! - `pipeline` — commands allowed mid-pipeline only (denied at position 0).
//! - `deny.<cmd>` — subcommand denylist within an allowed command.
//! - `guidance.<group>` — per-command hint messages for denied commands.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Serde helper that deserializes a string or a list of strings,
/// normalizing to `Vec<String>`.
///
/// Accepts both forms in TOML:
/// ```toml
/// build = "make"          # → vec!["make"]
/// build = ["make", "npm"] # → vec!["make", "npm"]
/// ```
#[derive(Debug, Clone)]
pub struct StringOrVec(pub Vec<String>);

impl<'de> Deserialize<'de> for StringOrVec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = StringOrVec;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string or list of strings")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<StringOrVec, E> {
                Ok(StringOrVec(vec![v.to_string()]))
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<StringOrVec, A::Error> {
                let mut vec = Vec::new();
                while let Some(s) = seq.next_element()? {
                    vec.push(s);
                }
                Ok(StringOrVec(vec))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

/// Format a list of build tools as a comma-separated backtick-quoted string.
///
/// e.g., `["make", "npm"]` → `` `make`, `npm` ``.
fn format_build_list(tools: &[String]) -> String {
    tools
        .iter()
        .map(|t| format!("`{t}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// TOML shape for a single `[commands.guidance.<group>]` entry.
///
/// Each group maps a set of commands to a guidance message. The `build`
/// group is special: it uses per-context message templates instead of a
/// single `message` string. Detection: if `message` is absent and any
/// `message_*` field is present, or if the group name is `"build"`, it's
/// treated as a build group.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct GuidanceGroup {
    /// Static hint message (e.g., `"Use {EDIT} for surgical edits"`).
    /// Absent for the `build` and `redirect` groups.
    pub message: Option<String>,
    /// Catenary subcommand to redirect to (e.g., `"grep"`, `"glob"`).
    /// When set, denials show a short message + the command's `-h` output.
    pub redirect: Option<String>,
    /// Commands this guidance applies to.
    pub commands: Vec<String>,
    /// Build: message when user config has a build tool.
    pub message_default: Option<String>,
    /// Build: message when user config has no build tool.
    pub message_default_absent: Option<String>,
    /// Build: message when no `.catenary.toml` exists.
    pub message_noproject: Option<String>,
    /// Build: message when project config has a build tool.
    pub message_project: Option<String>,
    /// Build: message when project config has no build tool.
    pub message_project_absent: Option<String>,
    /// Build: message when cwd cannot be resolved.
    pub message_cwd_unknown: Option<String>,
}

impl GuidanceGroup {
    /// Whether this group uses build-style message templates.
    const fn is_build(&self) -> bool {
        self.message.is_none()
            && (self.message_default.is_some()
                || self.message_default_absent.is_some()
                || self.message_noproject.is_some()
                || self.message_project.is_some()
                || self.message_project_absent.is_some()
                || self.message_cwd_unknown.is_some())
    }
}

/// Resolved guidance for a single command, ready for use at denial time.
#[derive(Debug, Clone)]
pub enum GuidanceEntry {
    /// Static message (read, edit groups).
    Static(String),
    /// Build group — message constructed at denial time from cwd context.
    Build(BuildGuidance),
    /// Redirect to a Catenary command — short denial with `-h` output.
    Redirect {
        /// Catenary subcommand name (e.g., `"grep"`, `"glob"`).
        command: String,
    },
}

/// Build-specific guidance with per-context message templates.
///
/// All fields have sensible defaults. Template variables resolved at
/// denial time: `{BUILD}`, `{CWD}`, `{USERCONFIG}`, `{PROJCONFIG}`.
#[derive(Debug, Clone)]
pub struct BuildGuidance {
    /// Message when user config has a build tool configured.
    pub message_default: String,
    /// Message when user config has no build tool.
    pub message_default_absent: String,
    /// Message when no `.catenary.toml` was found for the cwd.
    pub message_noproject: String,
    /// Message when project config has a build tool configured.
    pub message_project: String,
    /// Message when project config has no build tool.
    pub message_project_absent: String,
    /// Message when cwd cannot be resolved.
    pub message_cwd_unknown: String,
}

#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "{BUILD}, {CWD}, {USERCONFIG}, {PROJCONFIG} are template variables, not format args"
)]
impl Default for BuildGuidance {
    fn default() -> Self {
        Self {
            message_default: "{USERCONFIG} has {BUILD} as the default build tool.".to_string(),
            message_default_absent: "{USERCONFIG} has not configured a build tool.".to_string(),
            message_noproject: "No `.catenary.toml` was found in {CWD}.".to_string(),
            message_project: "{PROJCONFIG} has {BUILD} as the configured build tool.".to_string(),
            message_project_absent: "{PROJCONFIG} has not configured a build tool.".to_string(),
            message_cwd_unknown: "Unable to resolve the current working directory. Consider \
                                  changing directory and retrying."
                .to_string(),
        }
    }
}

/// Top-level `[commands]` config section.
///
/// Deserialized from TOML. The `deny` field uses a nested table:
/// `[commands.deny]` with keys mapping to arrays of denied subcommands
/// (e.g., `git = ["grep", "ls-files"]`).
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(default)]
pub struct CommandsConfig {
    /// Deliberate opt-out — no enforcement, no hint notification.
    #[serde(default)]
    pub client_enforcement_only: bool,
    /// The project's build tool(s) (e.g., `"make"` or `["make", "npm"]`).
    pub build: Option<StringOrVec>,
    /// Commands the agent can run unconditionally.
    pub allow: Option<Vec<String>>,
    /// Commands allowed mid-pipeline only (denied at pipeline position 0).
    pub pipeline: Option<Vec<String>>,
    /// Subcommand denylist within allowed commands.
    /// Key = command name, value = list of denied subcommands.
    pub deny: Option<HashMap<String, Vec<String>>>,
    /// Per-command flag denylist. Scans all argument positions.
    /// Key = command name, value = list of denied flags.
    pub deny_flags: Option<HashMap<String, Vec<String>>>,
    /// Per-command guidance groups.
    /// Key = group name (e.g., `"read"`, `"build"`), value = group config.
    pub guidance: Option<HashMap<String, GuidanceGroup>>,
}

/// A resolved command set after merging user and project configs.
#[derive(Debug, Clone, Default)]
pub struct ResolvedCommands {
    /// Deliberate opt-out — no enforcement, no hint notification.
    pub client_enforcement_only: bool,
    /// User-level default build tools (from user config, no root context).
    ///
    /// Used as fallback when `cwd` doesn't match any root in `build`.
    pub default_build: Vec<String>,
    /// Per-root build tools. Key = workspace root path, value = build tool names.
    ///
    /// Populated by [`merge_project_commands`](Self::merge_project_commands).
    /// The evaluator looks up `cwd` → root (longest prefix match) → build tools.
    pub build: HashMap<PathBuf, Vec<String>>,
    /// Commands the agent can run unconditionally.
    pub allow: HashSet<String>,
    /// Commands allowed mid-pipeline only.
    pub pipeline: HashSet<String>,
    /// Subcommand denylist within allowed commands.
    /// Key = command name, value = set of denied subcommands.
    pub deny: HashMap<String, HashSet<String>>,
    /// Per-command flag denylist. Scans all argument positions.
    /// Key = command name, value = set of denied flags.
    pub deny_flags: HashMap<String, HashSet<String>>,
    /// Per-command guidance messages for denial responses.
    /// Key = command name, value = guidance entry.
    pub guidance: HashMap<String, GuidanceEntry>,
}

impl ResolvedCommands {
    /// Merge a user-level config *source* layer into this resolved set.
    ///
    /// Layers later config sources (e.g. user config + an explicit
    /// `CATENARY_CONFIG` override); it is **not** the project-config path —
    /// project `.catenary.toml` enforcement is ignored (see
    /// [`merge_project_commands`](Self::merge_project_commands)). Each field
    /// overwrites when present in the layer: `allow` and `pipeline` are
    /// replaced (not unioned), `deny` entries are merged per-command, `build`
    /// is stored as `default_build` (user-level, no root context), and
    /// `guidance` groups are flattened into per-command entries.
    pub fn merge(&mut self, layer: &CommandsConfig) {
        if layer.client_enforcement_only {
            self.client_enforcement_only = true;
        }
        if let Some(ref build) = layer.build {
            self.default_build.clone_from(&build.0);
        }
        if let Some(ref allow) = layer.allow {
            self.allow = allow.iter().cloned().collect();
        }
        if let Some(ref pipeline) = layer.pipeline {
            self.pipeline = pipeline.iter().cloned().collect();
        }
        if let Some(ref deny) = layer.deny {
            for (cmd, subs) in deny {
                self.deny
                    .entry(cmd.clone())
                    .or_default()
                    .extend(subs.iter().cloned());
            }
        }
        if let Some(ref deny_flags) = layer.deny_flags {
            for (cmd, flags) in deny_flags {
                self.deny_flags
                    .entry(cmd.clone())
                    .or_default()
                    .extend(flags.iter().cloned());
            }
        }
        if let Some(ref groups) = layer.guidance {
            self.guidance = flatten_guidance(groups);
        }
    }

    /// Merge per-root project **build** tools into this user-level baseline.
    ///
    /// Command **enforcement** — `allow`/`pipeline`/`deny`/`deny_flags` — is
    /// **user-level only**: it is taken verbatim from `self`, and a project
    /// `.catenary.toml` cannot relax or replace it.
    /// The filter resolves daemon-globally (`Session::merged_commands` reads the
    /// *shared* `LspClientManager`'s daemon-wide roots + project configs), so
    /// honoring project enforcement keys would let one session's repo change the
    /// filter that *every* connected session sees. Only `build` is per-root and
    /// benign — it names a build tool and relaxes nothing. See DESIGN Decision 5
    /// Correction (ticket 15); ignored project keys are surfaced via
    /// [`PROJECT_IGNORED_COMMAND_KEYS`] at config-load time.
    ///
    /// For each root the project's `build` overrides the user default; roots
    /// without a project `build` fall back to
    /// [`default_build`](Self::default_build). Disabled roots contribute their
    /// `build` like enabled roots.
    ///
    /// Returns a new `ResolvedCommands` identical to `self` except for the
    /// per-root [`build`](Self::build) map.
    #[must_use]
    pub fn merge_project_commands(
        &self,
        roots: &[PathBuf],
        project_commands: &HashMap<PathBuf, CommandsConfig>,
    ) -> Self {
        let mut build: HashMap<PathBuf, Vec<String>> = HashMap::new();
        for root in roots {
            // build: project overrides user default for this root; everything
            // else is enforcement and stays user-level (taken from `self`).
            let root_build = project_commands
                .get(root)
                .and_then(|p| p.build.as_ref())
                .map_or(&self.default_build[..], |sv| &sv.0);
            if !root_build.is_empty() {
                build.insert(root.clone(), root_build.to_vec());
            }
        }

        Self {
            build,
            ..self.clone()
        }
    }

    /// Look up the build tools for a given `cwd`.
    ///
    /// Finds the root whose path is the longest prefix of `cwd` and returns
    /// that root's build tools. Falls back to [`default_build`](Self::default_build)
    /// when no root matches (e.g., no session or `cwd` outside all roots).
    /// Returns an empty slice when no build tools are configured.
    #[must_use]
    pub fn build_for_cwd(&self, cwd: Option<&Path>) -> &[String] {
        if let Some(cwd) = cwd
            && let Some(tools) = self
                .build
                .iter()
                .filter(|(root, _)| cwd.starts_with(root))
                .max_by_key(|(root, _)| root.as_os_str().len())
                .map(|(_, tools)| tools.as_slice())
        {
            return tools;
        }
        &self.default_build
    }

    /// Whether the allowlist is active (has at least one allowed command,
    /// pipeline command, or build tool).
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self.client_enforcement_only
            && (!self.allow.is_empty()
                || !self.pipeline.is_empty()
                || !self.build.is_empty()
                || !self.default_build.is_empty())
    }

    /// Look up guidance for a denied command.
    #[must_use]
    pub fn guidance_for(&self, cmd: &str) -> Option<&GuidanceEntry> {
        self.guidance.get(cmd)
    }
}

/// Flatten guidance groups into a per-command lookup map.
///
/// Each group's `commands` list is expanded so every command maps to the
/// group's guidance entry. The `build` group is detected by `is_build()`
/// (no `message`, has `message_*` fields) or by the group name `"build"`.
fn flatten_guidance(groups: &HashMap<String, GuidanceGroup>) -> HashMap<String, GuidanceEntry> {
    let mut map = HashMap::new();
    for (name, group) in groups {
        let entry = if name == "build" || group.is_build() {
            let defaults = BuildGuidance::default();
            GuidanceEntry::Build(BuildGuidance {
                message_default: group
                    .message_default
                    .clone()
                    .unwrap_or(defaults.message_default),
                message_default_absent: group
                    .message_default_absent
                    .clone()
                    .unwrap_or(defaults.message_default_absent),
                message_noproject: group
                    .message_noproject
                    .clone()
                    .unwrap_or(defaults.message_noproject),
                message_project: group
                    .message_project
                    .clone()
                    .unwrap_or(defaults.message_project),
                message_project_absent: group
                    .message_project_absent
                    .clone()
                    .unwrap_or(defaults.message_project_absent),
                message_cwd_unknown: group
                    .message_cwd_unknown
                    .clone()
                    .unwrap_or(defaults.message_cwd_unknown),
            })
        } else if let Some(ref redirect) = group.redirect {
            GuidanceEntry::Redirect {
                command: redirect.clone(),
            }
        } else if let Some(ref msg) = group.message {
            GuidanceEntry::Static(msg.clone())
        } else {
            // No message, not build, not redirect — skip this group.
            continue;
        };
        for cmd in &group.commands {
            map.insert(cmd.clone(), entry.clone());
        }
    }
    map
}

/// Context for resolving build guidance at denial time.
pub struct BuildContext<'a> {
    /// User config file path.
    pub user_config_path: &'a str,
    /// User-level default build tools.
    pub default_build: &'a [String],
    /// Whether a project config was found for the cwd.
    pub has_project_config: bool,
    /// Project config file path (if found).
    pub project_config_path: Option<&'a str>,
    /// Project-level build tools for the cwd's root.
    pub project_build: &'a [String],
    /// Whether cwd could be resolved.
    pub cwd_resolved: bool,
    /// The resolved working directory path (for context in messages).
    pub resolved_cwd_path: Option<&'a str>,
}

impl BuildGuidance {
    /// Resolve build guidance into a hint string for the denial response.
    ///
    /// Returns one or two lines depending on context. Empty-string templates
    /// suppress their line.
    #[must_use]
    #[allow(
        clippy::literal_string_with_formatting_args,
        reason = "{BUILD}, {CWD}, {USERCONFIG}, {PROJCONFIG} are template variables, not format args"
    )]
    pub fn resolve(&self, ctx: &BuildContext<'_>) -> String {
        if !ctx.cwd_resolved {
            return self.message_cwd_unknown.clone();
        }

        let mut lines = Vec::new();

        // User default build tool line.
        let user_line = if ctx.default_build.is_empty() {
            self.message_default_absent
                .replace("{USERCONFIG}", ctx.user_config_path)
        } else {
            let build_list = format_build_list(ctx.default_build);
            self.message_default
                .replace("{BUILD}", &build_list)
                .replace("{USERCONFIG}", ctx.user_config_path)
        };
        if !user_line.is_empty() {
            lines.push(user_line);
        }

        // Project config build tool line.
        let proj_path = ctx.project_config_path.unwrap_or(".catenary.toml");
        let cwd_display = ctx.resolved_cwd_path.unwrap_or("the working directory");
        let project_line = if !ctx.has_project_config {
            self.message_noproject.replace("{CWD}", cwd_display)
        } else if !ctx.project_build.is_empty() {
            let build_list = format_build_list(ctx.project_build);
            self.message_project
                .replace("{BUILD}", &build_list)
                .replace("{PROJCONFIG}", proj_path)
        } else {
            self.message_project_absent
                .replace("{PROJCONFIG}", proj_path)
        };
        if !project_line.is_empty() {
            lines.push(project_line);
        }

        lines.join("\n")
    }
}

/// Validate a `CommandsConfig`, returning all errors found.
///
/// Checks for:
/// - `client_enforcement_only` with active config fields
/// - Overlap between `allow` and `pipeline`
/// - `deny` keys not present in `allow`
/// - Empty `allow` or `pipeline` entries
/// - Empty `deny` subcommand entries
#[allow(clippy::too_many_lines, reason = "validation covers many fields")]
pub fn validate(config: &CommandsConfig) -> (Vec<String>, Vec<String>) {
    let mut errors = Vec::new();
    let warnings = Vec::new();

    // client_enforcement_only with active fields is contradictory
    if config.client_enforcement_only
        && (config.allow.is_some()
            || config.pipeline.is_some()
            || config.deny.is_some()
            || config.deny_flags.is_some()
            || config.build.is_some())
    {
        errors.push(
            "[commands] `client_enforcement_only = true` with `allow`, `pipeline`, \
             `deny`, `deny_flags`, or `build` is contradictory — \
             opt-out means no enforcement"
                .to_string(),
        );
    }

    // Collect allow/pipeline/build sets for cross-checks
    let allow_set: HashSet<&str> = config
        .allow
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(String::as_str)
        .collect();
    let pipeline_set: HashSet<&str> = config
        .pipeline
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(String::as_str)
        .collect();
    let build_set: HashSet<&str> = config
        .build
        .as_ref()
        .map_or(&[] as &[String], |sv| &sv.0)
        .iter()
        .map(String::as_str)
        .collect();

    // Check for overlap between allow and pipeline
    if let Some(ref pipeline) = config.pipeline {
        for cmd in pipeline {
            if allow_set.contains(cmd.as_str()) {
                errors.push(format!(
                    "[commands] '{cmd}' appears in both `allow` and `pipeline` — \
                     a command can only be in one list",
                ));
            }
        }
    }

    // deny keys must be in allow (can't deny subcommands of a non-allowed command)
    if let Some(ref deny) = config.deny {
        for cmd in deny.keys() {
            if !allow_set.contains(cmd.as_str()) {
                errors.push(format!(
                    "[commands] deny.{cmd} references '{cmd}' which is not in `allow` — \
                     can only deny subcommands of allowed commands",
                ));
            }
        }
    }

    // Empty strings in allow
    if let Some(ref allow) = config.allow {
        for cmd in allow {
            if cmd.is_empty() {
                errors.push("[commands] `allow` contains an empty string".to_string());
            }
        }
    }

    // Empty strings in pipeline
    if let Some(ref pipeline) = config.pipeline {
        for cmd in pipeline {
            if cmd.is_empty() {
                errors.push("[commands] `pipeline` contains an empty string".to_string());
            }
        }
    }

    // Empty deny subcommand entries
    if let Some(ref deny) = config.deny {
        for (cmd, subs) in deny {
            if subs.is_empty() {
                errors.push(format!(
                    "[commands] deny.{cmd} has an empty subcommand list",
                ));
            }
            for sub in subs {
                if sub.is_empty() {
                    errors.push(format!(
                        "[commands] deny.{cmd} contains an empty subcommand string",
                    ));
                }
            }
        }
    }

    // Empty build list or entries
    if let Some(ref build) = config.build {
        if build.0.is_empty() {
            errors.push("[commands] `build` is an empty list".to_string());
        }
        for b in &build.0 {
            if b.is_empty() {
                errors.push("[commands] `build` contains an empty string".to_string());
            }
        }
    }

    // deny_flags: keys must be in allow, pipeline, or build
    if let Some(ref deny_flags) = config.deny_flags {
        for (cmd, flags) in deny_flags {
            if !allow_set.contains(cmd.as_str())
                && !pipeline_set.contains(cmd.as_str())
                && !build_set.contains(cmd.as_str())
            {
                errors.push(format!(
                    "[commands] deny_flags.{cmd} references '{cmd}' which is not in \
                     `allow`, `pipeline`, or `build` — can only deny flags of \
                     permitted commands",
                ));
            }
            if flags.is_empty() {
                errors.push(format!(
                    "[commands] deny_flags.{cmd} has an empty flag list",
                ));
            }
            for flag in flags {
                if flag.is_empty() {
                    errors.push(format!(
                        "[commands] deny_flags.{cmd} contains an empty flag string",
                    ));
                }
            }
        }
    }

    // Guidance validation
    if let Some(ref groups) = config.guidance {
        for (name, group) in groups {
            if group.commands.is_empty() {
                errors.push(format!(
                    "[commands] guidance.{name} has an empty commands list",
                ));
            }
            for cmd in &group.commands {
                if cmd.is_empty() {
                    errors.push(format!(
                        "[commands] guidance.{name} contains an empty command string",
                    ));
                }
            }
            // Non-build, non-redirect group must have a message
            if name != "build"
                && !group.is_build()
                && group.redirect.is_none()
                && group.message.is_none()
            {
                errors.push(format!("[commands] guidance.{name} has no `message` field"));
            }
        }
    }

    (errors, warnings)
}

/// `[commands]` keys that Catenary **ignores** in a *project* `.catenary.toml`.
///
/// Only `build` is honored at project scope — it is per-root and benign (it
/// names a build tool and relaxes nothing). Everything else is user-level
/// only: command enforcement
/// (`client_enforcement_only`/`allow`/`pipeline`/`deny`/`deny_flags`) and
/// denial `guidance`. The filter resolves daemon-globally
/// (`Session::merged_commands`), so honoring any of these at project scope would
/// change the filter *every* connected session sees. See
/// [`merge_project_commands`](ResolvedCommands::merge_project_commands) and
/// DESIGN Decision 5 Correction (ticket 15).
///
/// The project-config loader warns on **presence** of any of these keys
/// (detected on the raw TOML, not the parsed config) so an explicit `= false`
/// on a boolean is caught too — `client_enforcement_only = false` (a project
/// asking for enforcement the daemon won't grant) is silently ignored
/// otherwise, which is the dangerous direction. Value-based detection cannot
/// distinguish `= false` from absent.
pub const PROJECT_IGNORED_COMMAND_KEYS: &[&str] = &[
    "client_enforcement_only",
    "allow",
    "pipeline",
    "deny",
    "deny_flags",
    "guidance",
];

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::literal_string_with_formatting_args,
    reason = "tests use expect for readable assertions; template vars look like format args"
)]
mod tests {
    use super::*;

    #[test]
    fn default_commands_config() {
        let config = CommandsConfig::default();
        assert!(!config.client_enforcement_only);
        assert!(config.build.is_none());
        assert!(config.allow.is_none());
        assert!(config.pipeline.is_none());
        assert!(config.deny.is_none());
    }

    #[test]
    fn deserialize_empty_toml() {
        let config: CommandsConfig = toml::from_str("").expect("empty TOML");
        assert!(!config.client_enforcement_only);
        assert!(config.build.is_none());
        assert!(config.allow.is_none());
        assert!(config.pipeline.is_none());
        assert!(config.deny.is_none());
    }

    #[test]
    fn deserialize_full_config() {
        let config: CommandsConfig = toml::from_str(
            r#"
build = "make"
allow = ["git", "gh", "cp"]
pipeline = ["grep", "head", "tail"]

[deny]
git = ["grep", "ls-files"]
"#,
        )
        .expect("valid TOML");

        assert_eq!(
            config.build.as_ref().map(|b| &b.0[..]),
            Some(["make".to_string()].as_slice()),
        );
        assert_eq!(config.allow.as_ref().expect("allow").len(), 3);
        assert_eq!(config.pipeline.as_ref().expect("pipeline").len(), 3);
        let deny = config.deny.as_ref().expect("deny");
        assert_eq!(deny.get("git").expect("git deny").len(), 2);
    }

    #[test]
    fn deserialize_build_multi_value() {
        let config: CommandsConfig = toml::from_str(
            r#"
build = ["make", "npm"]
allow = ["git"]
"#,
        )
        .expect("valid TOML");

        let build = config.build.as_ref().expect("build");
        assert_eq!(build.0, vec!["make", "npm"]);
    }

    #[test]
    fn deserialize_deny_flags() {
        let config: CommandsConfig = toml::from_str(
            r#"
allow = ["git", "make"]

[deny_flags]
make = ["-C"]
git = ["--no-verify", "--force"]
"#,
        )
        .expect("valid TOML");

        let deny_flags = config.deny_flags.as_ref().expect("deny_flags");
        assert_eq!(deny_flags.get("make").expect("make").len(), 1);
        assert_eq!(deny_flags.get("git").expect("git").len(), 2);
    }

    #[test]
    fn deserialize_client_enforcement_only() {
        let config: CommandsConfig =
            toml::from_str("client_enforcement_only = true").expect("valid TOML");
        assert!(config.client_enforcement_only);
    }

    #[test]
    fn legacy_allow_file_redirects_key_is_ignored() {
        // ws38 ticket 05 retired `allow_file_redirects` (writes now resolve-or-
        // deny at the resolver, not per user config). Lenient parsing silently
        // ignores the legacy key — a stale config still loads, it just has no
        // effect. Mirrors the `outline_threshold` retirement (`49a31a2`).
        let config: CommandsConfig =
            toml::from_str("allow = [\"git\"]\nallow_file_redirects = true")
                .expect("legacy key is ignored, not an error");
        assert_eq!(config.allow.as_ref().expect("allow").len(), 1);
    }

    #[test]
    fn resolve_single_layer() {
        let layer = CommandsConfig {
            build: Some(StringOrVec(vec!["make".to_string()])),
            allow: Some(vec!["git".to_string(), "gh".to_string()]),
            pipeline: Some(vec!["grep".to_string()]),
            deny: Some(HashMap::from([(
                "git".to_string(),
                vec!["grep".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&layer);

        assert_eq!(resolved.default_build, vec!["make"]);
        assert!(resolved.allow.contains("git"));
        assert!(resolved.allow.contains("gh"));
        assert!(resolved.pipeline.contains("grep"));
        assert!(resolved.deny.get("git").expect("git").contains("grep"));
    }

    #[test]
    fn project_allow_replaces_user_allow() {
        let user = CommandsConfig {
            allow: Some(vec!["git".to_string(), "gh".to_string(), "cp".to_string()]),
            ..CommandsConfig::default()
        };
        let project = CommandsConfig {
            allow: Some(vec![
                "git".to_string(),
                "gh".to_string(),
                "kubectl".to_string(),
            ]),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        // Project replaces user's allow list (sequential merge)
        assert!(resolved.allow.contains("git"));
        assert!(resolved.allow.contains("gh"));
        assert!(resolved.allow.contains("kubectl"));
        assert!(!resolved.allow.contains("cp"));
    }

    #[test]
    fn project_build_overrides_user() {
        let user = CommandsConfig {
            build: Some(StringOrVec(vec!["make".to_string()])),
            allow: Some(vec!["git".to_string()]),
            ..CommandsConfig::default()
        };
        let project = CommandsConfig {
            build: Some(StringOrVec(vec!["npm".to_string()])),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert_eq!(resolved.default_build, vec!["npm"]);
        // User's allow preserved (project didn't specify allow)
        assert!(resolved.allow.contains("git"));
    }

    #[test]
    fn deny_entries_merge_across_layers() {
        let user = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny: Some(HashMap::from([(
                "git".to_string(),
                vec!["grep".to_string()],
            )])),
            ..CommandsConfig::default()
        };
        let project = CommandsConfig {
            deny: Some(HashMap::from([(
                "git".to_string(),
                vec!["ls-files".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        let git_deny = resolved.deny.get("git").expect("git deny");
        assert!(git_deny.contains("grep"));
        assert!(git_deny.contains("ls-files"));
    }

    #[test]
    fn client_enforcement_only_sticky() {
        let user = CommandsConfig {
            client_enforcement_only: true,
            ..CommandsConfig::default()
        };
        let project = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&user);
        resolved.merge(&project);

        assert!(resolved.client_enforcement_only);
    }

    #[test]
    fn is_active_with_allow() {
        let resolved = ResolvedCommands {
            allow: HashSet::from(["git".to_string()]),
            ..ResolvedCommands::default()
        };
        assert!(resolved.is_active());
    }

    #[test]
    fn is_active_with_pipeline() {
        let resolved = ResolvedCommands {
            pipeline: HashSet::from(["grep".to_string()]),
            ..ResolvedCommands::default()
        };
        assert!(resolved.is_active());
    }

    #[test]
    fn is_active_with_default_build() {
        let resolved = ResolvedCommands {
            default_build: vec!["make".to_string()],
            ..ResolvedCommands::default()
        };
        assert!(resolved.is_active());
    }

    #[test]
    fn is_active_with_per_root_build() {
        let resolved = ResolvedCommands {
            build: HashMap::from([(PathBuf::from("/project"), vec!["make".to_string()])]),
            ..ResolvedCommands::default()
        };
        assert!(resolved.is_active());
    }

    #[test]
    fn is_active_empty() {
        let resolved = ResolvedCommands::default();
        assert!(!resolved.is_active());
    }

    #[test]
    fn is_active_client_enforcement_only() {
        let resolved = ResolvedCommands {
            client_enforcement_only: true,
            allow: HashSet::from(["git".to_string()]),
            ..ResolvedCommands::default()
        };
        assert!(!resolved.is_active());
    }

    // ── Validation tests ────────────────────────────────────────────

    #[test]
    fn validate_valid_config() {
        let config = CommandsConfig {
            build: Some(StringOrVec(vec!["make".to_string()])),
            allow: Some(vec!["git".to_string(), "gh".to_string()]),
            pipeline: Some(vec!["grep".to_string(), "head".to_string()]),
            deny: Some(HashMap::from([(
                "git".to_string(),
                vec!["grep".to_string(), "ls-files".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let (errors, warnings) = validate(&config);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    #[test]
    fn validate_client_enforcement_only_with_allow() {
        let config = CommandsConfig {
            client_enforcement_only: true,
            allow: Some(vec!["git".to_string()]),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("client_enforcement_only"));
    }

    #[test]
    fn validate_allow_pipeline_overlap() {
        let config = CommandsConfig {
            allow: Some(vec!["grep".to_string(), "git".to_string()]),
            pipeline: Some(vec!["grep".to_string()]),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("grep"));
        assert!(errors[0].contains("allow"));
        assert!(errors[0].contains("pipeline"));
    }

    #[test]
    fn validate_deny_not_in_allow() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny: Some(HashMap::from([(
                "sqlite3".to_string(),
                vec!["-cmd".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("sqlite3"));
        assert!(errors[0].contains("not in `allow`"));
    }

    #[test]
    fn validate_empty_allow_entry() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string(), String::new()]),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty string"));
    }

    #[test]
    fn validate_empty_pipeline_entry() {
        let config = CommandsConfig {
            pipeline: Some(vec![String::new()]),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty string"));
    }

    #[test]
    fn validate_empty_deny_subcommand_list() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny: Some(HashMap::from([("git".to_string(), vec![])])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty subcommand list"));
    }

    #[test]
    fn validate_empty_deny_subcommand_string() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny: Some(HashMap::from([("git".to_string(), vec![String::new()])])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty subcommand string"));
    }

    #[test]
    fn validate_empty_build_list() {
        let config = CommandsConfig {
            build: Some(StringOrVec(vec![])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty list"));
    }

    #[test]
    fn validate_empty_build_string() {
        let config = CommandsConfig {
            build: Some(StringOrVec(vec![String::new()])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty string"));
    }

    #[test]
    fn validate_deny_flags_key_not_in_any_list() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny_flags: Some(HashMap::from([(
                "sqlite3".to_string(),
                vec!["-cmd".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("deny_flags.sqlite3"));
        assert!(errors[0].contains("not in `allow`, `pipeline`, or `build`"));
    }

    #[test]
    fn validate_deny_flags_key_in_allow_ok() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny_flags: Some(HashMap::from([(
                "git".to_string(),
                vec!["--no-verify".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert!(
            errors.is_empty(),
            "deny_flags key in allow is valid: {errors:?}"
        );
    }

    #[test]
    fn validate_deny_flags_key_in_pipeline_ok() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            pipeline: Some(vec!["jq".to_string()]),
            deny_flags: Some(HashMap::from([(
                "jq".to_string(),
                vec!["--rawfile".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert!(
            errors.is_empty(),
            "deny_flags key in pipeline is valid: {errors:?}",
        );
    }

    #[test]
    fn validate_deny_flags_key_in_build_ok() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            build: Some(StringOrVec(vec!["make".to_string()])),
            deny_flags: Some(HashMap::from([(
                "make".to_string(),
                vec!["-C".to_string()],
            )])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert!(
            errors.is_empty(),
            "deny_flags key in build is valid: {errors:?}",
        );
    }

    #[test]
    fn validate_deny_flags_empty_list() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny_flags: Some(HashMap::from([("git".to_string(), vec![])])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty flag list"));
    }

    #[test]
    fn validate_deny_flags_empty_string() {
        let config = CommandsConfig {
            allow: Some(vec!["git".to_string()]),
            deny_flags: Some(HashMap::from([("git".to_string(), vec![String::new()])])),
            ..CommandsConfig::default()
        };

        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty flag string"));
    }

    #[test]
    fn validate_only_client_enforcement_only() {
        let config = CommandsConfig {
            client_enforcement_only: true,
            ..CommandsConfig::default()
        };

        let (errors, warnings) = validate(&config);
        assert!(errors.is_empty(), "unexpected errors: {errors:?}");
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
    }

    // ── Project config merge tests ─────────────────────────────────

    #[test]
    fn merge_project_build_only() {
        // Project enforcement keys (allow/pipeline/deny/deny_flags) are
        // user-level only and must be ignored — only `build` overrides
        // per-root. See DESIGN Decision 5 Correction (ticket 15).
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into(), "gh".into()]),
            pipeline: Some(vec!["grep".into()]),
            deny: Some(HashMap::from([("git".into(), vec!["push".into()])])),
            build: Some(StringOrVec(vec!["make".into()])),
            ..CommandsConfig::default()
        });

        let root = PathBuf::from("/project/a");
        let project_commands = HashMap::from([(
            root.clone(),
            CommandsConfig {
                allow: Some(vec!["kubectl".into()]),
                pipeline: Some(vec!["jq".into()]),
                deny: Some(HashMap::from([("git".into(), vec!["ls-files".into()])])),
                deny_flags: Some(HashMap::from([("git".into(), vec!["--no-verify".into()])])),
                build: Some(StringOrVec(vec!["npm".into()])),
                ..CommandsConfig::default()
            },
        )]);

        let merged = user.merge_project_commands(std::slice::from_ref(&root), &project_commands);

        // Enforcement is exactly the user's — every project value ignored.
        assert!(merged.allow.contains("git"));
        assert!(merged.allow.contains("gh"));
        assert!(!merged.allow.contains("kubectl"), "project allow ignored");
        assert!(merged.pipeline.contains("grep"));
        assert!(!merged.pipeline.contains("jq"), "project pipeline ignored");
        let git_deny = merged.deny.get("git").expect("user git deny preserved");
        assert!(git_deny.contains("push"));
        assert!(!git_deny.contains("ls-files"), "project deny ignored");
        assert!(merged.deny_flags.is_empty(), "project deny_flags ignored");

        // Only `build` is honored per-root.
        assert_eq!(
            merged.build.get(&root).map(Vec::as_slice),
            Some(["npm".to_string()].as_slice()),
        );
    }

    #[test]
    fn project_ignored_command_keys_cover_everything_but_build() {
        // `build` is the one key honored at project scope.
        assert!(
            !PROJECT_IGNORED_COMMAND_KEYS.contains(&"build"),
            "`build` must stay honored at project scope",
        );
        // Every other `[commands]` key — enforcement and guidance — is ignored,
        // so the loader warns on its presence (the `= false` boolean cases
        // included; see the const docs).
        for key in [
            "client_enforcement_only",
            "allow",
            "pipeline",
            "deny",
            "deny_flags",
            "guidance",
        ] {
            assert!(
                PROJECT_IGNORED_COMMAND_KEYS.contains(&key),
                "{key} should be ignored at project scope",
            );
        }
    }

    #[test]
    fn merge_project_disabled_root_contributes_build() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            build: Some(StringOrVec(vec!["make".into()])),
            ..CommandsConfig::default()
        });

        let root = PathBuf::from("/disabled/root");
        let project_commands = HashMap::from([(
            root.clone(),
            CommandsConfig {
                build: Some(StringOrVec(vec!["npm".into()])),
                ..CommandsConfig::default()
            },
        )]);

        let merged = user.merge_project_commands(std::slice::from_ref(&root), &project_commands);
        assert_eq!(
            merged.build.get(&root).map(Vec::as_slice),
            Some(["npm".to_string()].as_slice()),
        );
    }

    #[test]
    fn merge_project_build_per_root_with_cwd() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            build: Some(StringOrVec(vec!["make".into()])),
            ..CommandsConfig::default()
        });

        let root_a = PathBuf::from("/project/a");
        let root_b = PathBuf::from("/project/b");
        let project_commands = HashMap::from([(
            root_a.clone(),
            CommandsConfig {
                build: Some(StringOrVec(vec!["npm".into()])),
                ..CommandsConfig::default()
            },
        )]);

        let merged = user.merge_project_commands(&[root_a, root_b], &project_commands);
        // Root A: npm (project override). Root B: make (user default).
        assert_eq!(
            merged.build_for_cwd(Some(Path::new("/project/a/src"))),
            &["npm".to_string()],
        );
        assert_eq!(
            merged.build_for_cwd(Some(Path::new("/project/b/lib"))),
            &["make".to_string()],
        );
    }

    #[test]
    fn merge_project_no_roots_returns_clone() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            build: Some(StringOrVec(vec!["make".into()])),
            ..CommandsConfig::default()
        });

        let merged = user.merge_project_commands(&[], &HashMap::new());
        assert!(merged.allow.contains("git"));
        assert_eq!(merged.default_build, vec!["make"]);
        assert!(merged.build.is_empty());
    }

    #[test]
    fn build_for_cwd_falls_back_to_default() {
        let resolved = ResolvedCommands {
            default_build: vec!["make".into()],
            ..ResolvedCommands::default()
        };
        // No roots in build map — falls back to default
        assert_eq!(
            resolved.build_for_cwd(Some(Path::new("/any/path"))),
            &["make".to_string()],
        );
        assert_eq!(resolved.build_for_cwd(None), &["make".to_string()]);
    }

    #[test]
    fn build_for_cwd_no_build_returns_empty() {
        let resolved = ResolvedCommands::default();
        assert!(
            resolved
                .build_for_cwd(Some(Path::new("/any/path")))
                .is_empty()
        );
        assert!(resolved.build_for_cwd(None).is_empty());
    }

    #[test]
    fn build_for_cwd_longest_prefix_match() {
        let resolved = ResolvedCommands {
            build: HashMap::from([
                (PathBuf::from("/project"), vec!["make".into()]),
                (PathBuf::from("/project/nested"), vec!["npm".into()]),
            ]),
            ..ResolvedCommands::default()
        };
        assert_eq!(
            resolved.build_for_cwd(Some(Path::new("/project/nested/src"))),
            &["npm".to_string()],
        );
        assert_eq!(
            resolved.build_for_cwd(Some(Path::new("/project/other"))),
            &["make".to_string()],
        );
    }

    #[test]
    fn merge_project_build_multi_value() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            build: Some(StringOrVec(vec!["make".into(), "npm".into()])),
            ..CommandsConfig::default()
        });

        assert_eq!(user.default_build, vec!["make", "npm"]);

        let root = PathBuf::from("/project");
        let project_commands = HashMap::from([(
            root.clone(),
            CommandsConfig {
                build: Some(StringOrVec(vec!["cargo".into()])),
                ..CommandsConfig::default()
            },
        )]);

        let merged = user.merge_project_commands(std::slice::from_ref(&root), &project_commands);
        // Project replaces user default for that root
        assert_eq!(
            merged.build.get(&root).map(Vec::as_slice),
            Some(["cargo".to_string()].as_slice()),
        );
        // Default preserved
        assert_eq!(merged.default_build, vec!["make", "npm"]);
    }

    // ── Guidance tests ─────────────────────────────────────────────

    #[test]
    fn deserialize_guidance_static() {
        let config: CommandsConfig = toml::from_str(
            r#"
allow = ["git"]

[guidance.scan]
message = "Use Catenary's grep tool instead"
commands = ["grep", "rg"]
"#,
        )
        .expect("valid TOML");

        let groups = config.guidance.as_ref().expect("guidance");
        assert_eq!(groups.len(), 1);
        let scan = groups.get("scan").expect("scan group");
        assert_eq!(
            scan.message.as_deref(),
            Some("Use Catenary's grep tool instead")
        );
        assert_eq!(scan.commands, vec!["grep", "rg"]);
    }

    #[test]
    fn deserialize_guidance_build() {
        let config: CommandsConfig = toml::from_str(
            r#"
allow = ["git"]

[guidance.build]
commands = ["cargo", "npm"]
message_default = "custom: {BUILD}"
"#,
        )
        .expect("valid TOML");

        let groups = config.guidance.as_ref().expect("guidance");
        let build = groups.get("build").expect("build group");
        assert!(
            build.message.is_none(),
            "build group should have no message"
        );
        assert_eq!(build.message_default.as_deref(), Some("custom: {BUILD}"),);
    }

    #[test]
    fn flatten_guidance_static() {
        let config = CommandsConfig {
            guidance: Some(HashMap::from([(
                "scan".to_string(),
                GuidanceGroup {
                    message: Some("Use grep tool".to_string()),
                    commands: vec!["grep".to_string(), "rg".to_string()],
                    ..GuidanceGroup::default()
                },
            )])),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&config);

        assert!(matches!(
            resolved.guidance.get("grep"),
            Some(GuidanceEntry::Static(msg)) if msg == "Use grep tool"
        ));
        assert!(matches!(
            resolved.guidance.get("rg"),
            Some(GuidanceEntry::Static(msg)) if msg == "Use grep tool"
        ));
    }

    #[test]
    fn flatten_guidance_build() {
        let config = CommandsConfig {
            guidance: Some(HashMap::from([(
                "build".to_string(),
                GuidanceGroup {
                    commands: vec!["cargo".to_string(), "npm".to_string()],
                    ..GuidanceGroup::default()
                },
            )])),
            ..CommandsConfig::default()
        };

        let mut resolved = ResolvedCommands::default();
        resolved.merge(&config);

        assert!(
            matches!(
                resolved.guidance.get("cargo"),
                Some(GuidanceEntry::Build(_))
            ),
            "cargo should map to Build guidance",
        );
        assert!(
            matches!(resolved.guidance.get("npm"), Some(GuidanceEntry::Build(_))),
            "npm should map to Build guidance",
        );
    }

    #[test]
    fn build_guidance_resolve_both_configured() {
        let bg = BuildGuidance::default();
        let result = bg.resolve(&BuildContext {
            user_config_path: "~/.config/catenary/config.toml",
            default_build: &["make".to_string()],
            has_project_config: true,
            project_config_path: Some(".catenary.toml"),
            project_build: &["npm".to_string()],
            cwd_resolved: true,
            resolved_cwd_path: Some("/project"),
        });
        assert!(
            result.contains("`make`"),
            "should mention user build tool: {result}"
        );
        assert!(
            result.contains("`npm`"),
            "should mention project build tool: {result}"
        );
    }

    #[test]
    fn build_guidance_resolve_no_project() {
        let bg = BuildGuidance::default();
        let result = bg.resolve(&BuildContext {
            user_config_path: "~/.config/catenary/config.toml",
            default_build: &["make".to_string()],
            has_project_config: false,
            project_config_path: None,
            project_build: &[],
            cwd_resolved: true,
            resolved_cwd_path: Some("/other/project"),
        });
        assert!(
            result.contains("`make`"),
            "should mention user build: {result}"
        );
        assert!(
            result.contains("/other/project"),
            "should mention resolved cwd: {result}",
        );
    }

    #[test]
    fn build_guidance_resolve_nothing_configured() {
        let bg = BuildGuidance::default();
        let result = bg.resolve(&BuildContext {
            user_config_path: "~/.config/catenary/config.toml",
            default_build: &[],
            has_project_config: false,
            project_config_path: None,
            project_build: &[],
            cwd_resolved: true,
            resolved_cwd_path: Some("/project"),
        });
        assert!(
            result.contains("not configured"),
            "should say not configured: {result}",
        );
    }

    #[test]
    fn build_guidance_resolve_cwd_unknown() {
        let bg = BuildGuidance::default();
        let result = bg.resolve(&BuildContext {
            user_config_path: "",
            default_build: &[],
            has_project_config: false,
            project_config_path: None,
            project_build: &[],
            cwd_resolved: false,
            resolved_cwd_path: None,
        });
        assert!(
            result.contains("Unable to resolve"),
            "should show cwd unknown message: {result}",
        );
    }

    #[test]
    fn build_guidance_custom_messages() {
        let bg = BuildGuidance {
            message_default: "custom: {BUILD}".to_string(),
            message_noproject: "no project".to_string(),
            ..BuildGuidance::default()
        };
        let result = bg.resolve(&BuildContext {
            user_config_path: "config.toml",
            default_build: &["make".to_string()],
            has_project_config: false,
            project_config_path: None,
            project_build: &[],
            cwd_resolved: true,
            resolved_cwd_path: Some("/project"),
        });
        assert!(
            result.contains("custom: `make`"),
            "custom message: {result}",
        );
        assert!(result.contains("no project"), "custom no-project: {result}");
    }

    #[test]
    fn build_guidance_empty_suppresses_line() {
        let bg = BuildGuidance {
            message_default: String::new(),
            message_noproject: "visible".to_string(),
            ..BuildGuidance::default()
        };
        let result = bg.resolve(&BuildContext {
            user_config_path: "",
            default_build: &["make".to_string()],
            has_project_config: false,
            project_config_path: None,
            project_build: &[],
            cwd_resolved: true,
            resolved_cwd_path: Some("/project"),
        });
        // Empty message_default suppresses the user-default line.
        assert_eq!(result, "visible");
    }

    #[test]
    fn build_guidance_multi_value() {
        let bg = BuildGuidance::default();
        let result = bg.resolve(&BuildContext {
            user_config_path: "config.toml",
            default_build: &["make".to_string(), "npm".to_string()],
            has_project_config: true,
            project_config_path: Some(".catenary.toml"),
            project_build: &["cargo".to_string(), "npm".to_string()],
            cwd_resolved: true,
            resolved_cwd_path: Some("/project"),
        });
        assert!(
            result.contains("`make`, `npm`"),
            "should format multi-value default: {result}",
        );
        assert!(
            result.contains("`cargo`, `npm`"),
            "should format multi-value project: {result}",
        );
    }

    #[test]
    fn guidance_preserved_through_project_merge() {
        let mut user = ResolvedCommands::default();
        user.merge(&CommandsConfig {
            allow: Some(vec!["git".into()]),
            guidance: Some(HashMap::from([(
                "scan".to_string(),
                GuidanceGroup {
                    message: Some("use grep tool".to_string()),
                    commands: vec!["grep".to_string()],
                    ..GuidanceGroup::default()
                },
            )])),
            ..CommandsConfig::default()
        });

        let root = PathBuf::from("/project");
        let merged = user.merge_project_commands(&[root], &HashMap::new());
        assert!(
            matches!(merged.guidance.get("grep"), Some(GuidanceEntry::Static(msg)) if msg == "use grep tool"),
            "guidance should survive project merge",
        );
    }

    // ── Guidance validation tests ──────────────────────────────────

    #[test]
    fn validate_guidance_empty_commands() {
        let config = CommandsConfig {
            guidance: Some(HashMap::from([(
                "scan".to_string(),
                GuidanceGroup {
                    message: Some("hint".to_string()),
                    commands: vec![],
                    ..GuidanceGroup::default()
                },
            )])),
            ..CommandsConfig::default()
        };
        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty commands list"));
    }

    #[test]
    fn validate_guidance_empty_command_string() {
        let config = CommandsConfig {
            guidance: Some(HashMap::from([(
                "scan".to_string(),
                GuidanceGroup {
                    message: Some("hint".to_string()),
                    commands: vec![String::new()],
                    ..GuidanceGroup::default()
                },
            )])),
            ..CommandsConfig::default()
        };
        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("empty command string"));
    }

    #[test]
    fn validate_guidance_no_message() {
        let config = CommandsConfig {
            guidance: Some(HashMap::from([(
                "scan".to_string(),
                GuidanceGroup {
                    commands: vec!["grep".to_string()],
                    ..GuidanceGroup::default()
                },
            )])),
            ..CommandsConfig::default()
        };
        let (errors, _) = validate(&config);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("no `message` field"));
    }

    #[test]
    fn validate_guidance_build_no_message_ok() {
        // Build group doesn't need a message field.
        let config = CommandsConfig {
            guidance: Some(HashMap::from([(
                "build".to_string(),
                GuidanceGroup {
                    commands: vec!["cargo".to_string()],
                    ..GuidanceGroup::default()
                },
            )])),
            ..CommandsConfig::default()
        };
        let (errors, _) = validate(&config);
        assert!(errors.is_empty(), "build group should be valid: {errors:?}");
    }

    // ── GuidanceGroup::is_build tests ───────────────────────────────

    #[test]
    fn is_build_with_message_is_false() {
        let g = GuidanceGroup {
            message: Some("custom message".into()),
            ..GuidanceGroup::default()
        };
        assert!(!g.is_build(), "group with 'message' is not build-style");
    }

    #[test]
    fn is_build_with_message_default() {
        let g = GuidanceGroup {
            message_default: Some("default".into()),
            ..GuidanceGroup::default()
        };
        assert!(g.is_build(), "message_default alone should be build-style");
    }

    #[test]
    fn is_build_with_message_default_absent() {
        let g = GuidanceGroup {
            message_default_absent: Some("absent".into()),
            ..GuidanceGroup::default()
        };
        assert!(g.is_build());
    }

    #[test]
    fn is_build_with_message_noproject() {
        let g = GuidanceGroup {
            message_noproject: Some("no project".into()),
            ..GuidanceGroup::default()
        };
        assert!(g.is_build());
    }

    #[test]
    fn is_build_with_message_project() {
        let g = GuidanceGroup {
            message_project: Some("project".into()),
            ..GuidanceGroup::default()
        };
        assert!(g.is_build());
    }

    #[test]
    fn is_build_with_message_project_absent() {
        let g = GuidanceGroup {
            message_project_absent: Some("absent".into()),
            ..GuidanceGroup::default()
        };
        assert!(g.is_build());
    }

    #[test]
    fn is_build_with_message_cwd_unknown() {
        let g = GuidanceGroup {
            message_cwd_unknown: Some("unknown".into()),
            ..GuidanceGroup::default()
        };
        assert!(g.is_build());
    }

    #[test]
    fn is_build_empty_is_false() {
        let g = GuidanceGroup::default();
        assert!(
            !g.is_build(),
            "empty group with no fields is not build-style",
        );
    }
}
