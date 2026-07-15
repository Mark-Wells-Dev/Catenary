// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Config-file health checks: the migration walk, unknown keys, validation,
//! unreferenced servers, duplicate extensions, and project-config warnings.
//!
//! Each function reads config state and returns typed [`Finding`]s. None writes
//! output — rendering is the caller's job (see [`crate::cli::doctor`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::health::{Finding, FindingCode, Severity};

/// Walk config source files for pre-namespacing / legacy-format entries and
/// return migration findings.
///
/// Reads each source as raw TOML (independent of [`Config::load`]) to detect the
/// pre-namespacing top-level `[server.*]`/`[language.*]`/`[linter.<name>]`
/// tables (renamed under `[lsp.*]`/`[linter.rule.*]` in linters ticket 04), the
/// removed `inherit` field, `[lsp.language.*]` entries inlining server
/// definition fields, and the removed `[commands]` denylist fields. Each
/// produces a warning finding whose `fix_it` carries the equivalent new-format
/// config as data.
///
/// A non-empty result lets the caller drop the self-referential "run catenary
/// doctor" pointer from a subsequent config-load error (feedback 08 finding 1).
#[must_use]
pub fn migration_findings(sources: &[PathBuf]) -> Vec<Finding> {
    let mut findings = Vec::new();

    for source in sources {
        let Ok(contents) = std::fs::read_to_string(source) else {
            continue;
        };
        let Ok(raw) = toml::from_str::<toml::Value>(&contents) else {
            continue;
        };

        // Pre-namespacing top-level definition tables (linters ticket 04).
        if let Some(table) = raw.get("server").and_then(toml::Value::as_table) {
            findings.push(namespace_rename(source, "server", "lsp.server", table));
        }
        if let Some(table) = raw.get("language").and_then(toml::Value::as_table) {
            findings.push(namespace_rename(source, "language", "lsp.language", table));
        }
        if let Some(linter_table) = raw.get("linter").and_then(toml::Value::as_table) {
            let old_defs: toml::map::Map<String, toml::Value> = linter_table
                .iter()
                .filter(|(k, v)| k.as_str() != "rule" && k.as_str() != "disable" && v.is_table())
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            if !old_defs.is_empty() {
                findings.push(namespace_rename(source, "linter", "linter.rule", &old_defs));
            }
        }

        // [lsp.language.*] entries with removed or stale fields.
        if let Some(table) = raw
            .get("lsp")
            .and_then(|v| v.get("language"))
            .and_then(toml::Value::as_table)
        {
            for (key, entry) in table {
                let Some(entry_table) = entry.as_table() else {
                    continue;
                };
                if entry_table.contains_key("inherit") {
                    let target = entry_table
                        .get("inherit")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("?");
                    findings.push(
                        Finding::new(
                            FindingCode::ConfigLanguageInherit,
                            Severity::Warning,
                            format!(
                                "{}: [lsp.language.{key}] uses the removed `inherit` field",
                                source.display(),
                            ),
                        )
                        .with_fix_it(format!(
                            "Copy the `servers` list from [lsp.language.{target}] into \
                             [lsp.language.{key}] instead.",
                        )),
                    );
                }

                let has_server_fields = crate::config::SERVER_DEF_KEYS
                    .iter()
                    .any(|k| entry_table.contains_key(*k));
                if has_server_fields {
                    findings.push(inline_server_migration(source, key, entry_table));
                }
            }
        }

        // [commands] entries with old denylist-format fields.
        if let Some(cmd_table) = raw.get("commands").and_then(toml::Value::as_table) {
            if cmd_table.contains_key("deny_when_first") {
                findings.push(
                    Finding::new(
                        FindingCode::ConfigLegacyCommandsField,
                        Severity::Warning,
                        format!(
                            "{}: [commands] uses the removed `deny_when_first` field",
                            source.display(),
                        ),
                    )
                    .with_fix_it(
                        "Catenary now uses an allowlist model. Run `catenary config` for the \
                         recommended template.",
                    ),
                );
            }

            if let Some(deny_table) = cmd_table.get("deny").and_then(toml::Value::as_table)
                && let Some((key, _)) = deny_table.iter().find(|(_, v)| v.is_str())
            {
                findings.push(
                    Finding::new(
                        FindingCode::ConfigLegacyCommandsField,
                        Severity::Warning,
                        format!(
                            "{}: [commands.deny.{key}] has a string value — the old \
                             guidance-string format is removed",
                            source.display(),
                        ),
                    )
                    .with_fix_it(
                        "`deny` now maps commands to arrays of denied subcommands \
                         (e.g., `git = [\"grep\", \"ls-files\"]`).",
                    ),
                );
            }
        }
    }

    findings
}

/// Build a namespace-rename finding whose fix-it lists every `[old] → [new]`
/// header, including nested sub-tables.
fn namespace_rename(
    source: &Path,
    old_root: &str,
    new_root: &str,
    table: &toml::map::Map<String, toml::Value>,
) -> Finding {
    let mut headers = Vec::new();
    for (name, value) in table {
        if let Some(sub) = value.as_table() {
            collect_table_headers(&format!("{old_root}.{name}"), sub, &mut headers);
        }
    }
    let fix_it = headers
        .iter()
        .map(|header| {
            // `header` is guaranteed to start with `old_root`; swap the prefix.
            let new_header = format!("{new_root}{}", &header[old_root.len()..]);
            format!("[{header}]  →  [{new_header}]")
        })
        .collect::<Vec<_>>()
        .join("\n");

    Finding::new(
        FindingCode::ConfigLegacyNamespace,
        Severity::Warning,
        format!(
            "{}: [{old_root}.*] moved under [{new_root}.*] (linters ticket 04) — \
             rename these table headers",
            source.display(),
        ),
    )
    .with_fix_it(fix_it)
}

/// Collects the header path for `table` and every nested sub-table, parent
/// first, into `headers`. `path` is the dotted TOML header for `table`.
fn collect_table_headers(
    path: &str,
    table: &toml::map::Map<String, toml::Value>,
    headers: &mut Vec<String>,
) {
    headers.push(path.to_string());
    for (key, value) in table {
        if let Some(sub) = value.as_table() {
            collect_table_headers(&format!("{path}.{key}"), sub, headers);
        }
    }
}

/// Build a finding for a `[lsp.language.*]` entry that inlines server definition
/// fields, with the split into `[lsp.language.*]` + `[lsp.server.*]` as fix-it.
fn inline_server_migration(
    source: &Path,
    key: &str,
    entry: &toml::map::Map<String, toml::Value>,
) -> Finding {
    let server_name = entry
        .get("command")
        .and_then(toml::Value::as_str)
        .unwrap_or(key);

    let server_fields: Vec<(&str, &toml::Value)> = crate::config::SERVER_DEF_KEYS
        .iter()
        .filter_map(|k| entry.get(*k).map(|v| (*k, v)))
        .collect();
    let lang_fields: Vec<(&str, &toml::Value)> = entry
        .iter()
        .filter(|(k, _)| !crate::config::SERVER_DEF_KEYS.contains(&k.as_str()))
        .map(|(k, v)| (k.as_str(), v))
        .collect();

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("[lsp.language.{key}]"));
    lines.push(format!("servers = [\"{server_name}\"]"));
    for (k, v) in &lang_fields {
        lines.push(format!("{k} = {v}"));
    }
    lines.push(String::new());
    lines.push(format!("[lsp.server.{server_name}]"));
    for (k, v) in &server_fields {
        lines.push(format!("{k} = {v}"));
    }

    Finding::new(
        FindingCode::ConfigLanguageInlinesServer,
        Severity::Warning,
        format!(
            "{}: [lsp.language.{key}] inlines server definition fields — split into \
             [lsp.language.*] + [lsp.server.*]",
            source.display(),
        ),
    )
    .with_fix_it(lines.join("\n"))
}

/// Warn per unknown key found in the config sources (misc 131).
///
/// Walks each source as raw TOML against the embedded user-config JSON Schema
/// ([`crate::config::schema::unknown_user_config_keys`]). Unknown keys warn,
/// never error: forward compatibility means an older binary reading a newer
/// config keeps working.
#[must_use]
pub fn unknown_key_findings(sources: &[PathBuf]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for source in sources {
        let Ok(contents) = std::fs::read_to_string(source) else {
            continue;
        };
        let Ok(raw) = toml::from_str::<toml::Value>(&contents) else {
            continue;
        };
        for unknown in crate::config::schema::unknown_user_config_keys(&raw) {
            let location = if unknown.location.is_empty() {
                "top level".to_string()
            } else {
                format!("in [{}]", unknown.location)
            };
            findings.push(Finding::new(
                FindingCode::ConfigUnknownKey,
                Severity::Warning,
                format!(
                    "{}: `{}` ({location}) is not a Catenary config key — remove it",
                    source.display(),
                    unknown.key,
                ),
            ));
        }
    }
    findings
}

/// Validation errors from [`Config::validate`], as error findings.
#[must_use]
pub fn validation_findings(config: &Config) -> Vec<Finding> {
    config
        .validate()
        .into_iter()
        .map(|err| Finding::new(FindingCode::ConfigValidationError, Severity::Error, err))
        .collect()
}

/// Quarantined-section findings (bug 110).
///
/// A config section that failed validation was defaulted out so the load
/// succeeded on the valid remainder; each such section produces one
/// [`Severity::Error`] finding naming the section and its error(s), with a fix-it
/// pointing the user at the fix. This is the doctor surface for the loud degrade:
/// `grep`/`glob` warn on stderr and the daemon fires one notification at boot,
/// but doctor is where the user reads the full error list.
#[must_use]
pub fn quarantine_findings(config: &Config) -> Vec<Finding> {
    config
        .quarantined
        .sections()
        .iter()
        .map(|section| {
            let errors = section.errors.join("; ");
            Finding::new(
                FindingCode::ConfigSectionQuarantined,
                Severity::Error,
                format!(
                    "[{}] quarantined — the section failed validation and was disabled: {errors}",
                    section.section,
                ),
            )
            .with_fix_it(format!(
                "Fix the errors above in [{}] to restore it. Until then its consumers \
                 degrade: for [commands], command filtering is OFF (or fails closed if \
                 `client_enforcement_only = true`).",
                section.section,
            ))
        })
        .collect()
}

/// Missing persisted-pin findings (misc 175).
///
/// Each `[roots] pinned` entry whose path is absent on disk (deleted repo,
/// unmounted volume) produces a [`Severity::Warning`] finding. The entry is
/// **kept** in the config — Catenary never rewrites the user's config outside an
/// explicit pin/unpin, so a transiently absent mount stays pinned — and this
/// surfaces it instead of silently discarding operator intent. The message names
/// the entry as authored (the `~`-prefixed spelling), and the fix-it points at
/// `catenary unpin` for a genuinely gone root.
#[must_use]
pub fn pinned_root_findings(config: &Config) -> Vec<Finding> {
    config
        .pinned_roots()
        .iter()
        .filter(|entry| {
            let expanded = crate::bridge::expand_tilde(entry);
            !Path::new(&expanded).exists()
        })
        .map(|entry| {
            Finding::new(
                FindingCode::ConfigPinnedRootMissing,
                Severity::Warning,
                format!("Pinned root '{entry}' is missing on disk — kept in config, not restored"),
            )
            .with_fix_it(format!(
                "If the root is gone for good, run `catenary unpin {entry}`; a \
                 transiently absent mount stays pinned and restores on its next boot."
            ))
        })
        .collect()
}

/// Unreferenced *user-defined* server findings.
///
/// An embedded default orphaned by a user `[lsp.language.*]` override is normal
/// operation, not user error (feedback 08 finding 2) — only user-defined
/// servers nothing routes to warn.
#[must_use]
pub fn unreferenced_server_findings(config: &Config) -> Vec<Finding> {
    unreferenced_user_servers(config)
        .into_iter()
        .map(|name| {
            Finding::new(
                FindingCode::ConfigUnreferencedServer,
                Severity::Warning,
                format!(
                    "Server '{name}' is defined but not referenced by any \
                     [lsp.language.*] entry"
                ),
            )
        })
        .collect()
}

/// User-defined servers that no `[lsp.language.*]` entry routes to, sorted.
///
/// Embedded defaults are exempt (feedback 08 finding 2 / misc 120).
#[must_use]
pub fn unreferenced_user_servers(config: &Config) -> Vec<&str> {
    let referenced: HashSet<&str> = config
        .language
        .values()
        .flat_map(|lc| lc.servers().iter().map(|b| b.name.as_str()))
        .collect();
    let defaults = crate::config::default_server_names();
    let mut unreferenced: Vec<&str> = config
        .server
        .keys()
        .filter(|name| !referenced.contains(name.as_str()) && !defaults.contains(name.as_str()))
        .map(String::as_str)
        .collect();
    unreferenced.sort_unstable();
    unreferenced
}

/// Duplicate-extension findings — an extension claimed by two languages.
#[must_use]
pub fn duplicate_extension_findings(config: &Config) -> Vec<Finding> {
    crate::bridge::filesystem_manager::ClassificationTables::find_duplicate_extensions(config)
        .into_iter()
        .map(|(ext, first, second)| {
            Finding::new(
                FindingCode::ConfigDuplicateExtension,
                Severity::Warning,
                format!(
                    "Extension '.{ext}' claimed by both [lsp.language.{first}] and \
                     [lsp.language.{second}] — first wins"
                ),
            )
        })
        .collect()
}

/// Leftover-launcher-args findings — a `[lsp.server.<key>]` whose `args`
/// contain the key itself.
///
/// The retired `command` field (misc 162) took a launcher and its `args` — e.g.
/// `command = "rustup"`, `args = ["run", "stable", "rust-analyzer"]`. The
/// migration teaching error forces `command` out, but `args` are free-form and
/// stay, so the daemon spawns `rust-analyzer run stable rust-analyzer` — the
/// server key with arguments written FOR the retired launcher — and the server
/// dies on the unknown arguments with nothing pointing at the config (bug 94).
///
/// The one known shape (`args` containing the server key) catches the real case
/// with zero false-positive cost; args are legitimately free-form, so this is a
/// [`Severity::Suggestion`], not an error.
#[must_use]
pub fn leftover_launcher_args_findings(config: &Config) -> Vec<Finding> {
    let mut names: Vec<&String> = config
        .server
        .iter()
        .filter(|(name, def)| def.args.iter().any(|arg| arg == name.as_str()))
        .map(|(name, _)| name)
        .collect();
    names.sort_unstable();
    names
        .into_iter()
        .map(|name| {
            Finding::new(
                FindingCode::ConfigLeftoverLauncherArgs,
                Severity::Suggestion,
                format!(
                    "[lsp.server.{name}] args contain '{name}' — this looks like \
                     launcher arguments left behind when the retired `command` field \
                     was removed (misc 162). The daemon now spawns '{name}' directly, \
                     so these args are passed to it. Drop the leftover launcher args"
                ),
            )
        })
        .collect()
}

/// Rewrite a config-load error for doctor's own render.
///
/// The migration walker's rename guidance prints directly above the error in
/// doctor, so the guard's "run `catenary doctor`" pointer is rewritten to point
/// at that guidance instead of back at the command the user is already inside
/// (feedback 08 finding 1). Other surfaces keep the pointer verbatim.
#[must_use]
pub fn rewrite_guard_pointer(rendered: &str) -> String {
    rendered.replace(
        crate::config::MIGRATION_GUIDANCE_POINTER,
        "See the rename guidance above.",
    )
}

/// The project `.catenary.toml` path for `project_root`, if it exists.
///
/// Returns `None` when the root cannot be canonicalized or the file is absent —
/// the signal the caller uses to skip the project-config section entirely.
#[must_use]
pub fn project_config_path(project_root: &Path) -> Option<PathBuf> {
    let resolved = project_root.canonicalize().ok()?;
    let config_path = resolved.join(".catenary.toml");
    config_path.exists().then_some(config_path)
}

/// Project `.catenary.toml` findings: removed toggles, load errors, summary,
/// per-root disable toggles, ignored enforcement keys, orphan servers, and
/// unresolved server references.
///
/// Returns an empty vec when no project config exists. `user_config` supplies
/// the combined server set for reference validation.
#[allow(clippy::too_many_lines, reason = "sequential per-section reporting")]
#[must_use]
pub fn project_config_findings(project_root: &Path, user_config: &Config) -> Vec<Finding> {
    let mut findings = Vec::new();

    let Some(config_path) = project_config_path(project_root) else {
        return findings;
    };
    let Ok(resolved) = project_root.canonicalize() else {
        return findings;
    };

    // Flag the removed `lsp`/`enabled` kill switch (workstream 34 ticket 00)
    // before parsing — `load_project_config` hard-errors on these keys.
    let removed_key = std::fs::read_to_string(&config_path)
        .ok()
        .and_then(|c| toml::from_str::<toml::Value>(&c).ok())
        .and_then(|raw| {
            if matches!(raw.get("lsp"), Some(v) if !v.is_table()) {
                Some("lsp")
            } else if raw.get("enabled").is_some() {
                Some("enabled")
            } else {
                None
            }
        });
    if let Some(key) = removed_key {
        findings.push(Finding::new(
            FindingCode::ProjectRemovedToggle,
            Severity::Error,
            format!(
                "bare `{key}` was removed — use a `[lsp]` table with `disable` \
                 (`lsp = false` becomes `[lsp]` / `disable = true`)"
            ),
        ));
    }

    match crate::config::load_project_config(&resolved) {
        Ok(Some(pc)) => {
            let lang_count = pc.language.len();
            let server_count = pc.server.len();
            findings.push(Finding::new(
                FindingCode::ProjectSummary,
                Severity::Ok,
                format!(
                    "{lang_count} language{}, {server_count} server{}",
                    if lang_count == 1 { "" } else { "s" },
                    if server_count == 1 { "" } else { "s" },
                ),
            ));

            for (section, set, note) in [
                (
                    "lsp",
                    pc.disable_lsp,
                    "no LSP servers, grep/glob enrichment, or LSP diagnostics",
                ),
                ("linter", pc.disable_lint, "no linter diagnostics"),
                (
                    "diagnostics",
                    pc.disable_diag,
                    "diagnostics surface off; LSP navigation kept",
                ),
            ] {
                if set {
                    findings.push(Finding::new(
                        FindingCode::ProjectDisableToggle,
                        Severity::Info,
                        format!("[{section}] disable — {note}"),
                    ));
                }
            }

            findings.extend(ignored_enforcement_findings(&config_path));

            for server_name in pc.server.keys() {
                // A def whose key already names a known server (misc 162: the key
                // IS the executable) is an override, not an orphan.
                if user_config.server.contains_key(server_name) {
                    continue;
                }
                let referenced_by_project = pc
                    .language
                    .values()
                    .any(|lc| lc.servers().iter().any(|b| b.name == *server_name));
                let referenced_by_user = user_config
                    .language
                    .values()
                    .any(|lc| lc.servers().iter().any(|b| b.name == *server_name));
                if !referenced_by_project && !referenced_by_user {
                    findings.push(Finding::new(
                        FindingCode::ProjectOrphanServer,
                        Severity::Warning,
                        format!(
                            "[lsp.server.{server_name}] defines a server no \
                             [lsp.language.*] references it"
                        ),
                    ));
                }
            }

            for (lang_key, lang_config) in &pc.language {
                for binding in lang_config.servers() {
                    if !pc.server.contains_key(&binding.name)
                        && !user_config.server.contains_key(&binding.name)
                    {
                        findings.push(Finding::new(
                            FindingCode::ProjectUnresolvedServerRef,
                            Severity::Error,
                            format!(
                                "[lsp.language.{lang_key}] references server '{}', but no \
                                 [lsp.server.{}] is defined in project or user config",
                                binding.name, binding.name,
                            ),
                        ));
                    }
                }
            }
        }
        Ok(None) => {}
        Err(e) => {
            findings.push(Finding::new(
                FindingCode::ProjectLoadError,
                Severity::Error,
                format!("{}: {e:#}", config_path.display()),
            ));
        }
    }

    findings
}

/// Ignored-enforcement-keys-at-project-scope finding.
///
/// Command enforcement resolves daemon-globally, so a project `.catenary.toml`
/// honors only `[commands] build`; every other `[commands]` key is ignored.
/// Detected on the raw TOML so an explicit `= false` on a boolean is caught too
/// (see [`crate::config::PROJECT_IGNORED_COMMAND_KEYS`]).
fn ignored_enforcement_findings(config_path: &Path) -> Vec<Finding> {
    let Ok(contents) = std::fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Ok(raw) = toml::from_str::<toml::Value>(&contents) else {
        return Vec::new();
    };
    let Some(table) = raw.get("commands").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    let ignored: Vec<&str> = crate::config::PROJECT_IGNORED_COMMAND_KEYS
        .iter()
        .copied()
        .filter(|key| table.contains_key(*key))
        .collect();
    if ignored.is_empty() {
        return Vec::new();
    }
    vec![
        Finding::new(
            FindingCode::ProjectIgnoredEnforcement,
            Severity::Warning,
            format!(
                "[commands] keys other than `build` are ignored at project scope ({}) — \
                 command enforcement is a daemon-wide, user-level decision",
                ignored.join(", "),
            ),
        )
        .with_fix_it(
            "Move these keys to your user config (~/.config/catenary/config.toml).".to_string(),
        ),
    ]
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::fs;

    /// The combined message + fix-it text of a finding, for substring assertions.
    fn rendered(finding: &Finding) -> String {
        finding.fix_it.as_ref().map_or_else(
            || finding.message.clone(),
            |fix| format!("{}\n{fix}", finding.message),
        )
    }

    // ── migration walk (bug 57) ─────────────────────────────────────

    #[test]
    fn migration_findings_walk_all_three_pre_namespacing_classes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = tmp.path().join("config.toml");
        fs::write(
            &cfg,
            "[server.rust-analyzer]\ncommand = \"rust-analyzer\"\n\n\
             [language.rust]\nservers = [\"rust-analyzer\"]\n\n\
             [linter.shellcheck]\ncommand = \"shellcheck\"\n",
        )
        .expect("write config");

        let findings = migration_findings(&[cfg]);
        let text = findings.iter().map(rendered).collect::<Vec<_>>().join("\n");

        assert!(
            findings
                .iter()
                .filter(|f| f.code == FindingCode::ConfigLegacyNamespace)
                .count()
                == 3,
            "one namespace finding per class, got:\n{text}",
        );
        assert!(
            text.contains("[server.rust-analyzer]  →  [lsp.server.rust-analyzer]"),
            "{text}"
        );
        assert!(
            text.contains("[language.rust]  →  [lsp.language.rust]"),
            "{text}"
        );
        assert!(
            text.contains("[linter.shellcheck]  →  [linter.rule.shellcheck]"),
            "{text}"
        );
    }

    #[test]
    fn migration_findings_silent_for_namespaced_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = tmp.path().join("config.toml");
        fs::write(
            &cfg,
            "[lsp.server.rust-analyzer]\ncommand = \"rust-analyzer\"\n\n\
             [lsp.language.rust]\nservers = [\"rust-analyzer\"]\n\n\
             [linter.rule.shellcheck]\ncommand = \"shellcheck\"\npatterns = [\"**/*.sh\"]\n",
        )
        .expect("write config");
        assert!(migration_findings(&[cfg]).is_empty());
    }

    // ── unknown keys (misc 131) ─────────────────────────────────────

    #[test]
    fn unknown_key_findings_name_dead_top_level_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = tmp.path().join("config.toml");
        fs::write(&cfg, "smart_wait = true\n").expect("write config");

        let findings = unknown_key_findings(&[cfg]);
        let finding = findings.first().expect("one finding");
        assert_eq!(finding.code, FindingCode::ConfigUnknownKey);
        assert!(
            finding.message.contains("`smart_wait` (top level)"),
            "got: {}",
            finding.message,
        );
    }

    #[test]
    fn unknown_key_findings_name_nested_typo() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = tmp.path().join("config.toml");
        fs::write(&cfg, "[icons]\ntypo_key = 1\n").expect("write config");

        let findings = unknown_key_findings(&[cfg]);
        let finding = findings.first().expect("one finding");
        assert!(
            finding.message.contains("`typo_key` (in [icons])"),
            "got: {}",
            finding.message,
        );
    }

    #[test]
    fn unknown_key_findings_silent_for_known_and_passthrough() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = tmp.path().join("config.toml");
        fs::write(
            &cfg,
            "[icons]\npreset = \"unicode\"\n\n\
             [lsp.server.rust-analyzer]\npath = \"/usr/local/bin/rust-analyzer\"\n\n\
             [lsp.server.rust-analyzer.initialization_options]\ncheck = { command = \"clippy\" }\n\n\
             [lsp.server.rust-analyzer.settings]\nanything = true\n",
        )
        .expect("write config");
        assert!(unknown_key_findings(&[cfg]).is_empty());
    }

    // ── guard pointer rewrite (feedback 08 finding 1) ───────────────

    #[test]
    fn guard_pointer_rewritten_for_doctor_render() {
        let msg = format!(
            "Failed to parse config file: /x/config.toml: config uses \
             pre-namespacing top-level keys — foo. {}",
            crate::config::MIGRATION_GUIDANCE_POINTER,
        );
        let rewritten = rewrite_guard_pointer(&msg);
        assert!(!rewritten.contains("catenary doctor"), "{rewritten}");
        assert!(
            rewritten.contains("See the rename guidance above."),
            "{rewritten}"
        );
    }

    // ── unreferenced-server scope (feedback 08 finding 2) ───────────

    #[test]
    fn unreferenced_exempts_embedded_defaults() {
        let defaults = crate::config::default_server_names();
        let a_default = defaults
            .iter()
            .next()
            .expect("embedded defaults must be non-empty")
            .clone();

        let mut config = Config::default();
        config
            .server
            .insert(a_default.clone(), crate::config::ServerDef::default());
        config.server.insert(
            "user-orphan-xyz".to_string(),
            crate::config::ServerDef::default(),
        );

        let unref = unreferenced_user_servers(&config);
        assert!(unref.contains(&"user-orphan-xyz"), "{unref:?}");
        assert!(!unref.iter().any(|n| *n == a_default), "{unref:?}");
    }

    // ── ignored enforcement keys at project scope ───────────────────

    #[test]
    fn ignored_enforcement_flags_project_scope_keys_including_false_bools() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(".catenary.toml");
        // `= false` (a project asking for enforcement) is the silent direction —
        // detection is on the raw key presence, so it is caught too.
        fs::write(
            &path,
            "[commands]\nclient_enforcement_only = false\nbuild = [\"make\"]\n",
        )
        .expect("write project config");

        let findings = ignored_enforcement_findings(&path);
        let finding = findings.first().expect("one finding");
        assert_eq!(finding.code, FindingCode::ProjectIgnoredEnforcement);
        assert_eq!(finding.severity, Severity::Warning);
        assert!(
            finding.message.contains("client_enforcement_only"),
            "got: {}",
            finding.message,
        );
    }

    #[test]
    fn ignored_enforcement_silent_for_build_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join(".catenary.toml");
        fs::write(&path, "[commands]\nbuild = [\"make\"]\n").expect("write project config");
        assert!(ignored_enforcement_findings(&path).is_empty());
    }

    // ── leftover launcher args (bug 94) ─────────────────────────────

    #[test]
    fn leftover_launcher_args_flags_key_in_own_args() {
        // The exact pre-162 shape: rust-analyzer carrying the retired launcher's
        // `args = ["run", "stable", "rust-analyzer"]` — the key names itself.
        let mut config = Config::default();
        config.server.insert(
            "rust-analyzer".to_string(),
            crate::config::ServerDef {
                args: vec![
                    "run".to_string(),
                    "stable".to_string(),
                    "rust-analyzer".to_string(),
                ],
                ..Default::default()
            },
        );

        let findings = leftover_launcher_args_findings(&config);
        let finding = findings.first().expect("one finding");
        assert_eq!(finding.code, FindingCode::ConfigLeftoverLauncherArgs);
        assert_eq!(finding.severity, Severity::Suggestion);
        assert!(
            finding.message.contains("[lsp.server.rust-analyzer]")
                && finding.message.contains("misc 162"),
            "names the server and the migration: {}",
            finding.message,
        );
    }

    #[test]
    fn leftover_launcher_args_silent_for_ordinary_args() {
        // Free-form args that do not name the key are legitimate — no finding.
        let mut config = Config::default();
        config.server.insert(
            "rust-analyzer".to_string(),
            crate::config::ServerDef {
                args: vec!["--log-file".to_string(), "/tmp/ra.log".to_string()],
                ..Default::default()
            },
        );
        assert!(leftover_launcher_args_findings(&config).is_empty());
    }

    // ── persisted-pin missing path (misc 175) ────────────────────────

    /// A `Config` whose `[roots] pinned` list is exactly `entries`.
    fn config_with_pins(entries: &[&str]) -> Config {
        let mut config = Config::default();
        config.roots = Some(crate::config::RootsConfig {
            companions: None,
            pinned: entries.iter().map(|s| (*s).to_string()).collect(),
        });
        config
    }

    #[test]
    fn pinned_root_findings_flag_a_missing_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let present = tmp.path().join("present");
        fs::create_dir(&present).expect("mkdir present");
        let missing = tmp.path().join("gone");
        let config = config_with_pins(&[
            present.to_str().expect("utf8"),
            missing.to_str().expect("utf8"),
        ]);

        let findings = pinned_root_findings(&config);
        assert_eq!(findings.len(), 1, "only the missing path warns");
        let f = &findings[0];
        assert_eq!(f.code, FindingCode::ConfigPinnedRootMissing);
        assert_eq!(f.severity, Severity::Warning);
        assert!(
            f.message.contains(missing.to_str().expect("utf8")),
            "names the missing entry: {}",
            f.message
        );
        assert!(
            rendered(f).contains("catenary unpin"),
            "fix-it points at unpin: {}",
            rendered(f)
        );
    }

    #[test]
    fn pinned_root_findings_silent_when_all_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = config_with_pins(&[tmp.path().to_str().expect("utf8")]);
        assert!(pinned_root_findings(&config).is_empty());
    }

    #[test]
    fn pinned_root_findings_empty_without_pins() {
        assert!(pinned_root_findings(&Config::default()).is_empty());
    }
}
