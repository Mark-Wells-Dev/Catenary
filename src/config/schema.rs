// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! JSON Schema for Catenary's config files (misc 133).
//!
//! Two schemas are shipped under `schemas/`, both `schemars`-generated from the
//! same serde structs the loader deserializes so they can never drift from the
//! runtime shape (a byte-for-byte freshness gate lives in the tests below,
//! mirroring the shipped-teaching-fallback pattern in
//! [`crate::cli::teaching`]):
//!
//! - `schemas/config.json` — the full user config
//!   (`~/.config/catenary/config.toml`), generated from
//!   [`RawConfig`](super::parse) with `additionalProperties: false` so a dead
//!   key like `smart_wait` is flagged in-editor.
//! - `schemas/catenary-project.json` — the project `.catenary.toml`, narrowed to
//!   the honored keys (`[lsp]`, `[linter]`, `[diagnostics]`, `[commands].build`)
//!   with the user-scope-only enforcement keys marked `deprecated` so the editor
//!   warning matches the runtime warning.
//!
//! The pass-through subtrees (`initialization_options`, `settings`, `env`) stay
//! open — server-defined content is never Catenary's to validate.
//!
//! Delivery is validator-neutral (misc 133): the artifacts are standard JSON
//! Schema. [`install_toml_schema_association`] wires them into the taplo server
//! Catenary spawns for zero-setup validation; the shipped `catenary config`
//! template carries a `#:schema` directive pointing at the published URL; and
//! the docs pipeline republishes the artifacts at that URL.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;
use tracing::debug;

use super::Config;

/// The committed full-user-config schema, embedded for offline delivery.
///
/// Byte-for-byte identical to the regenerated schema — enforced by
/// `schemas_are_fresh`.
pub const USER_CONFIG_SCHEMA: &str = include_str!("../../schemas/config.json");

/// The committed project-config schema (`.catenary.toml`), embedded for offline
/// delivery. Freshness-gated by `schemas_are_fresh`.
pub const PROJECT_CONFIG_SCHEMA: &str = include_str!("../../schemas/catenary-project.json");

/// Published URL of the full-user-config schema (docs-site gh-pages).
///
/// The `#:schema` directive in the `catenary config` template points here.
pub const USER_SCHEMA_URL: &str = "https://twowells.github.io/catenary/schemas/config.json";

/// Published URL of the project-config schema.
pub const PROJECT_SCHEMA_URL: &str =
    "https://twowells.github.io/catenary/schemas/catenary-project.json";

/// Wires Catenary's JSON Schemas into the `taplo` server it spawns.
///
/// Materializes the embedded schemas to a stable cache path and associates them
/// with the config file paths so every user gets live validation + unknown-key
/// squiggles on `~/.config/catenary/config.toml` and `.catenary.toml` with zero
/// setup.
///
/// The schemas resolve to a local `file://` URI (embedded via `include_str!`,
/// written to [`crate::paths::cache_dir`] at daemon start), so the common path
/// never touches the network. Best-effort: a filesystem error is traced at
/// `debug` and leaves the config untouched — schema validation is a nicety, not
/// a core function, and taplo still works without it.
///
/// The association is per-server, keyed on the `taplo` server name. A user who
/// has replaced their taplo settings keeps them — the auto associations are
/// deep-merged *under* the user's settings, so the user always wins on a
/// conflict while still gaining associations for keys they did not set.
pub fn install_toml_schema_association(config: &mut Config) {
    match materialize_schemas() {
        Ok((user_path, project_path)) => associate_taplo(config, &user_path, &project_path),
        Err(err) => {
            debug!("could not materialize Catenary config schemas for taplo: {err:#}");
        }
    }
}

/// Cache directory holding the materialized schema copies.
fn schema_dir() -> PathBuf {
    crate::paths::cache_dir().join("catenary").join("schemas")
}

/// Writes the embedded schemas to the cache dir, returning their paths.
fn materialize_schemas() -> Result<(PathBuf, PathBuf)> {
    let dir = schema_dir();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create schema cache dir {}", dir.display()))?;

    let user = dir.join("config.json");
    let project = dir.join("catenary-project.json");
    write_if_changed(&user, USER_CONFIG_SCHEMA)?;
    write_if_changed(&project, PROJECT_CONFIG_SCHEMA)?;
    Ok((user, project))
}

/// Writes `contents` to `path` only when the on-disk copy differs, so an
/// unchanged schema does not churn the cache mtime on every daemon start.
fn write_if_changed(path: &Path, contents: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|existing| existing == contents) {
        return Ok(());
    }
    std::fs::write(path, contents).with_context(|| format!("write schema {}", path.display()))?;
    Ok(())
}

/// Injects the taplo `evenBetterToml.schema.associations` settings that point
/// the config paths at the local schema files.
///
/// taplo pulls its config via `workspace/configuration` under the
/// `evenBetterToml` section (its default `configurationSection`), which
/// Catenary answers from `ServerDef.settings`. The association keys are regexes
/// matched against the document URI; the values are the local schema URIs.
fn associate_taplo(config: &mut Config, user_path: &Path, project_path: &Path) {
    let Some(taplo) = config.server.get_mut("taplo") else {
        return;
    };

    let associations = json!({
        "evenBetterToml": {
            "schema": {
                "enabled": true,
                "associations": {
                    ".*/catenary/config\\.toml$": file_uri(user_path),
                    ".*\\.catenary\\.toml$": file_uri(project_path),
                }
            }
        }
    });

    taplo.settings = Some(match taplo.settings.as_ref() {
        // Deep-merge with the auto associations as the *base* so the user's own
        // settings win on any conflicting key.
        Some(existing) => super::merge::deep_merge(&associations, existing),
        None => associations,
    });
}

/// Builds a `file://` URI for a local schema path.
fn file_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use std::collections::HashMap;

    use schemars::JsonSchema;
    use serde_json::Value;

    use super::super::commands::{GuidanceGroup, PROJECT_IGNORED_COMMAND_KEYS, StringOrVec};
    use super::super::parse::RawConfig;
    use super::super::{LanguageConfig, LinterConfig, ServerDef};
    use super::{PROJECT_CONFIG_SCHEMA, PROJECT_SCHEMA_URL, USER_CONFIG_SCHEMA, USER_SCHEMA_URL};

    // ── Project-config schema mirror ─────────────────────────────────────
    //
    // `.catenary.toml` is parsed by walking `toml::Value` (no single serde
    // struct), so these schema-only mirrors describe its honored shape. They
    // reference the shared leaf structs (`ServerDef`/`LanguageConfig`/
    // `LinterConfig`/`StringOrVec`/`GuidanceGroup`), so the leaf field sets stay
    // SSOT; only the thin section wrappers live here, and
    // `project_schema_top_level_keys_are_honored` guards the wrapper key set
    // against `PROJECT_CONFIG_ALLOWED_KEYS`.

    /// Schema-only mirror of `.catenary.toml` (project config).
    #[derive(JsonSchema)]
    #[schemars(deny_unknown_fields)]
    #[allow(dead_code, reason = "fields exist only to shape the generated schema")]
    struct ProjectConfigSchema {
        /// LSP feeder: per-root server/language definitions and the `disable`
        /// toggle.
        lsp: Option<ProjectLspSchema>,
        /// Linter feeder: per-root linter rules and the `disable` toggle.
        linter: Option<ProjectLinterSchema>,
        /// Diagnostics surface: the `disable` toggle.
        diagnostics: Option<ProjectDiagnosticsSchema>,
        /// Command/build config. Only `build` is honored at project scope; the
        /// enforcement keys are user-scope only (marked deprecated).
        commands: Option<ProjectCommandsSchema>,
    }

    #[derive(JsonSchema)]
    #[schemars(deny_unknown_fields)]
    #[allow(dead_code, reason = "fields exist only to shape the generated schema")]
    struct ProjectLspSchema {
        /// Drops the LSP feeder for this root.
        disable: Option<bool>,
        /// Server definitions (`[lsp.server.*]`).
        server: Option<HashMap<String, ServerDef>>,
        /// Language definitions (`[lsp.language.*]`).
        language: Option<HashMap<String, LanguageConfig>>,
    }

    #[derive(JsonSchema)]
    #[schemars(deny_unknown_fields)]
    #[allow(dead_code, reason = "fields exist only to shape the generated schema")]
    struct ProjectLinterSchema {
        /// Drops the linter feeder for this root.
        disable: Option<bool>,
        /// Standalone-linter definitions (`[linter.rule.*]`).
        rule: Option<HashMap<String, LinterConfig>>,
    }

    #[derive(JsonSchema)]
    #[schemars(deny_unknown_fields)]
    #[allow(dead_code, reason = "fields exist only to shape the generated schema")]
    struct ProjectDiagnosticsSchema {
        /// Suppresses the diagnostics surface for this root.
        disable: Option<bool>,
    }

    #[derive(JsonSchema)]
    #[schemars(deny_unknown_fields)]
    #[allow(dead_code, reason = "fields exist only to shape the generated schema")]
    struct ProjectCommandsSchema {
        /// The project's build tool(s) — the one command key honored at project
        /// scope (per-root and benign).
        build: Option<StringOrVec>,
        client_enforcement_only: Option<bool>,
        allow: Option<Vec<String>>,
        pipeline: Option<Vec<String>>,
        deny: Option<HashMap<String, Vec<String>>>,
        deny_flags: Option<HashMap<String, Vec<String>>>,
        allow_flags: Option<HashMap<String, Vec<String>>>,
        script_hosts: Option<Vec<String>>,
        guidance: Option<HashMap<String, GuidanceGroup>>,
    }

    /// Renders one schema `Value` to the committed on-disk form: pretty JSON
    /// (2-space, alphabetical keys via `serde_json::Value`'s `BTreeMap`) plus a
    /// trailing newline.
    fn to_pretty(value: &Value) -> String {
        let mut out = serde_json::to_string_pretty(value).expect("schema serializes");
        out.push('\n');
        out
    }

    /// Overwrites the root metadata (`$id`/`title`/`description`) on a generated
    /// schema.
    fn decorate(value: &mut Value, id: &str, title: &str, description: &str) {
        if let Some(obj) = value.as_object_mut() {
            obj.insert("$id".to_owned(), json_str(id));
            obj.insert("title".to_owned(), json_str(title));
            obj.insert("description".to_owned(), json_str(description));
        }
    }

    fn json_str(s: &str) -> Value {
        Value::String(s.to_owned())
    }

    /// Generates the full-user-config schema (`schemas/config.json`).
    fn render_user_schema() -> String {
        let root = schemars::schema_for!(RawConfig);
        let mut value = serde_json::to_value(&root).expect("root schema serializes");
        decorate(
            &mut value,
            USER_SCHEMA_URL,
            "Catenary user configuration",
            "Schema for the Catenary user config (~/.config/catenary/config.toml). \
             Generated from the serde config structs (misc 133).",
        );
        to_pretty(&value)
    }

    /// Generates the project-config schema (`schemas/catenary-project.json`),
    /// marking the user-scope-only command keys `deprecated`.
    fn render_project_schema() -> String {
        let root = schemars::schema_for!(ProjectConfigSchema);
        let mut value = serde_json::to_value(&root).expect("root schema serializes");
        decorate(
            &mut value,
            PROJECT_SCHEMA_URL,
            "Catenary project configuration",
            "Schema for a project-local .catenary.toml. Only build and the LSP/linter/\
             diagnostics feeder toggles are honored at project scope; user-scope-only \
             enforcement keys are marked deprecated (misc 133).",
        );
        mark_project_enforcement_deprecated(&mut value);
        to_pretty(&value)
    }

    /// Marks every user-scope-only `[commands]` key `deprecated` in the project
    /// schema, sourced from [`PROJECT_IGNORED_COMMAND_KEYS`] so it cannot drift
    /// from the runtime warn-and-ignore set.
    fn mark_project_enforcement_deprecated(value: &mut Value) {
        let props = value
            .get_mut("definitions")
            .and_then(|d| d.get_mut("ProjectCommandsSchema"))
            .and_then(|c| c.get_mut("properties"))
            .and_then(Value::as_object_mut)
            .expect("ProjectCommandsSchema properties present in generated schema");
        for key in PROJECT_IGNORED_COMMAND_KEYS {
            let prop = props
                .get_mut(*key)
                .and_then(Value::as_object_mut)
                .expect("project commands enforcement key present in schema");
            prop.insert("deprecated".to_owned(), Value::Bool(true));
            prop.insert(
                "description".to_owned(),
                json_str(
                    "User-scope only. Command enforcement resolves daemon-wide, so at \
                     project scope (.catenary.toml) Catenary ignores this key with a \
                     warning. Move it to your user config \
                     (~/.config/catenary/config.toml).",
                ),
            );
        }
    }

    /// Path to a committed schema artifact under the crate root.
    fn schema_path(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("schemas")
            .join(name)
    }

    /// Regenerate-and-compare freshness gate. Set `CATENARY_BLESS_SCHEMAS=1` to
    /// rewrite the committed artifacts from the structs instead of asserting.
    #[test]
    fn schemas_are_fresh() {
        let user = render_user_schema();
        let project = render_project_schema();

        if std::env::var_os("CATENARY_BLESS_SCHEMAS").is_some() {
            std::fs::write(schema_path("config.json"), &user).expect("write config.json");
            std::fs::write(schema_path("catenary-project.json"), &project)
                .expect("write catenary-project.json");
            return;
        }

        assert_eq!(
            USER_CONFIG_SCHEMA, user,
            "schemas/config.json is stale — regenerate with \
             `CATENARY_BLESS_SCHEMAS=1 make test T=schemas_are_fresh`"
        );
        assert_eq!(
            PROJECT_CONFIG_SCHEMA, project,
            "schemas/catenary-project.json is stale — regenerate with \
             `CATENARY_BLESS_SCHEMAS=1 make test T=schemas_are_fresh`"
        );
    }

    /// The full-user schema closes the top-level key set, so a dead key such as
    /// `smart_wait = true` (the misc-131 repro) is flagged in-editor.
    #[test]
    fn user_schema_rejects_unknown_top_level_key() {
        let value: Value = serde_json::from_str(USER_CONFIG_SCHEMA).expect("valid schema JSON");
        assert_eq!(
            value.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "top-level additionalProperties must be false so unknown keys are flagged"
        );
        let props = value
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties");
        assert!(
            !props.contains_key("smart_wait"),
            "smart_wait is not a real key — it must not appear in the schema"
        );
    }

    /// The pass-through subtrees stay open: `initialization_options`/`settings`
    /// accept any value, and `env` accepts arbitrary keys.
    #[test]
    fn passthrough_subtrees_stay_open() {
        let value: Value = serde_json::from_str(USER_CONFIG_SCHEMA).expect("valid schema JSON");
        let server_props = value
            .pointer("/definitions/ServerDef/properties")
            .and_then(Value::as_object)
            .expect("ServerDef properties");

        for open in ["initialization_options", "settings"] {
            let schema = server_props.get(open).expect("passthrough field present");
            assert!(
                is_open(schema),
                "{open} must accept arbitrary content (got {schema})"
            );
        }

        // `env` is a map: arbitrary keys, string values — never a closed object.
        let env = server_props.get("env").expect("env present");
        assert_ne!(
            env.get("additionalProperties"),
            Some(&Value::Bool(false)),
            "env must allow arbitrary keys"
        );
    }

    /// A schema fragment is "open" when it is `true`, `{}`, or explicitly does
    /// not forbid additional properties.
    fn is_open(schema: &Value) -> bool {
        match schema {
            Value::Bool(b) => *b,
            Value::Object(obj) => {
                obj.is_empty() || obj.get("additionalProperties") != Some(&Value::Bool(false))
            }
            _ => false,
        }
    }

    /// The project schema marks every user-scope-only enforcement key
    /// `deprecated` (so `.catenary.toml` with `allow = […]` warns in-editor)
    /// while leaving `build` a normal, honored key.
    #[test]
    fn project_schema_deprecates_enforcement_keys() {
        let value: Value = serde_json::from_str(PROJECT_CONFIG_SCHEMA).expect("valid schema JSON");
        let props = value
            .pointer("/definitions/ProjectCommandsSchema/properties")
            .and_then(Value::as_object)
            .expect("ProjectCommandsSchema properties");

        for key in PROJECT_IGNORED_COMMAND_KEYS {
            let prop = props.get(*key).expect("enforcement key present");
            assert_eq!(
                prop.get("deprecated"),
                Some(&Value::Bool(true)),
                "project key `{key}` must be marked deprecated"
            );
        }

        let build = props.get("build").expect("build present");
        assert_ne!(
            build.get("deprecated"),
            Some(&Value::Bool(true)),
            "build is honored at project scope — it must not be deprecated"
        );
    }

    /// The project schema's honored top-level sections match the loader's
    /// allow-list, so the two surfaces cannot disagree.
    #[test]
    fn project_schema_top_level_keys_are_honored() {
        let value: Value = serde_json::from_str(PROJECT_CONFIG_SCHEMA).expect("valid schema JSON");
        let mut schema_keys: Vec<String> = value
            .get("properties")
            .and_then(Value::as_object)
            .expect("properties")
            .keys()
            .cloned()
            .collect();
        schema_keys.sort();

        let mut honored = super::super::parse::PROJECT_CONFIG_ALLOWED_KEYS.to_vec();
        honored.sort_unstable();

        assert_eq!(
            schema_keys,
            honored.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            "project schema sections must match PROJECT_CONFIG_ALLOWED_KEYS"
        );
    }

    /// Both artifacts are draft-07 (or later) JSON Schema and carry the
    /// published `$id` used by the `#:schema` directive and `SchemaStore` entry.
    #[test]
    fn schemas_carry_published_ids() {
        let user: Value = serde_json::from_str(USER_CONFIG_SCHEMA).expect("valid schema JSON");
        assert_eq!(
            user.get("$id").and_then(Value::as_str),
            Some(USER_SCHEMA_URL)
        );
        let project: Value =
            serde_json::from_str(PROJECT_CONFIG_SCHEMA).expect("valid schema JSON");
        assert_eq!(
            project.get("$id").and_then(Value::as_str),
            Some(PROJECT_SCHEMA_URL)
        );
    }
}
