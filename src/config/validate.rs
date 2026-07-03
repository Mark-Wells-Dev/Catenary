// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Configuration validation.

use super::Config;
use crate::lsp::glob::{LspGlob, is_glob_pattern};
use crate::source::Source;

/// Validate the merged config, returning all errors found.
///
/// Returns an empty vec when the config is valid.
#[must_use]
pub fn validate(config: &Config) -> Vec<String> {
    let mut errors = Vec::new();

    // Validate language entries
    for (key, lang_config) in &config.language {
        // Entries that have servers OR no classification are expected to
        // have a non-empty servers list.  Classification-only entries
        // (from the default config) are valid without servers.
        if lang_config.servers().is_empty() && !lang_config.has_classification() {
            errors.push(format!(
                "Language '{key}' has no `servers` and no classification fields — \
                 every language entry must specify a servers list or classification"
            ));
        }

        // Validate server references
        for binding in lang_config.servers() {
            if !config.server.contains_key(&binding.name) {
                errors.push(format!(
                    "Language '{key}' references server '{}', \
                     but no [lsp.server.{}] is defined",
                    binding.name, binding.name,
                ));
            }
        }

        // Validate classification fields — no empty strings
        if let Some(ref exts) = lang_config.extensions {
            for ext in exts {
                if ext.is_empty() {
                    errors.push(format!(
                        "Language '{key}' has an empty string in `extensions`"
                    ));
                }
            }
        }
        if let Some(ref fnames) = lang_config.filenames {
            for fname in fnames {
                if fname.is_empty() {
                    errors.push(format!(
                        "Language '{key}' has an empty string in `filenames`"
                    ));
                }
            }
        }
        if let Some(ref shebangs) = lang_config.shebangs {
            for shebang in shebangs {
                if shebang.is_empty() {
                    errors.push(format!(
                        "Language '{key}' has an empty string in `shebangs`"
                    ));
                }
            }
        }
        if let Some(ref markers) = lang_config.root_markers {
            for marker in markers {
                if marker.is_empty() {
                    errors.push(format!(
                        "Language '{key}' has an empty string in `root_markers`"
                    ));
                } else if is_glob_pattern(marker)
                    && let Err(e) = LspGlob::new(marker)
                {
                    errors.push(format!(
                        "Language '{key}' has an invalid glob in `root_markers`: \
                         '{marker}' — {e}"
                    ));
                }
            }
        }
    }

    // Validate server definitions
    for (name, server_def) in &config.server {
        if server_def.command.is_empty() {
            errors.push(format!(
                "Server '{name}' has an empty `command` — \
                 server definitions must specify a command"
            ));
        }

        // Validate file_patterns — each must be a valid glob, no empty strings
        for pattern in &server_def.file_patterns {
            if pattern.is_empty() {
                errors.push(format!(
                    "Server '{name}' has an empty string in `file_patterns`"
                ));
            } else if let Err(e) = LspGlob::new(pattern) {
                errors.push(format!(
                    "Server '{name}' has an invalid glob in `file_patterns`: \
                     '{pattern}' — {e}"
                ));
            }
        }

        // Validate the optional provisional code-band regex (linters ticket 05).
        // Compiled lazily at weight resolution, so check it here.
        if let Some(pat) = &server_def.provisional
            && let Err(e) = regex::Regex::new(pat)
        {
            errors.push(format!(
                "Server '{name}' has an invalid regex in `provisional`: '{pat}' — {e}"
            ));
        }
    }

    validate_linters(config, &mut errors);

    errors
}

/// Validates `[linter.rule.*]` definitions, appending any errors (workstream 34
/// tickets 01/04).
///
/// Each linter must have a non-empty `command`, and every routing pattern must
/// be a non-empty, valid glob.
fn validate_linters(config: &Config, errors: &mut Vec<String>) {
    for (name, linter) in &config.linter {
        if linter.command.is_empty() {
            errors.push(format!(
                "Linter '{name}' has an empty `command` — \
                 linter definitions must specify a command"
            ));
        }

        for pattern in &linter.patterns {
            if pattern.is_empty() {
                errors.push(format!("Linter '{name}' has an empty string in `patterns`"));
            } else if let Err(e) = LspGlob::new(pattern) {
                errors.push(format!(
                    "Linter '{name}' has an invalid glob in `patterns`: '{pattern}' — {e}"
                ));
            }
        }

        for shebang in &linter.shebangs {
            if shebang.is_empty() {
                errors.push(format!("Linter '{name}' has an empty string in `shebangs`"));
            }
        }
    }
}

/// Warns about orphan `[lsp.server.*]` entries in a project config.
///
/// A project server def is an orphan if it has spawn fields (`command`)
/// but neither the project's `[lsp.language.*]` nor the user's `[lsp.language.*]`
/// references it. Settings-only overrides (no `command`) are not orphans
/// — they override user-level server settings for this root.
pub fn warn_orphan_project_servers(
    project: &super::ProjectConfig,
    user_config: &Config,
    root: &std::path::Path,
) {
    for (server_name, server_def) in &project.server {
        // Settings-only override — not an orphan.
        if server_def.command.is_empty() {
            continue;
        }

        let referenced_by_project = project
            .language
            .values()
            .any(|lc| lc.servers().iter().any(|b| b.name == *server_name));

        let referenced_by_user = user_config
            .language
            .values()
            .any(|lc| lc.servers().iter().any(|b| b.name == *server_name));

        if !referenced_by_project && !referenced_by_user {
            tracing::warn!(
                source = Source::ConfigValidation.as_str(),
                root = %root.display(),
                server = server_name.as_str(),
                "Project config at {}: [lsp.server.{server_name}] has a `command` \
                 but no [lsp.language.*] references it — this server will never be spawned",
                root.display(),
            );
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::config::{LanguageConfig, ProjectConfig, ServerBinding, ServerDef};
    use std::collections::HashMap;
    use std::path::PathBuf;

    #[test]
    fn test_orphan_server_warning() {
        // A project server with command but no language references is an orphan.
        let mut project = ProjectConfig::default();
        project.server.insert(
            "unused-server".to_string(),
            ServerDef {
                command: "unused-server-bin".to_string(),
                args: Vec::new(),
                ..ServerDef::default()
            },
        );

        let user_config = Config::default();
        let root = PathBuf::from("/test");

        // The function emits a tracing::warn — we verify it runs without panic.
        // In a real test you could use a tracing subscriber to capture warnings.
        warn_orphan_project_servers(&project, &user_config, &root);
    }

    #[test]
    fn test_orphan_server_settings_only_no_warning() {
        // A project server with empty command (settings-only override) is not
        // an orphan — it just overrides the user-level server's settings.
        let mut project = ProjectConfig::default();
        project.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                settings: Some(serde_json::json!({"key": "value"})),
                ..ServerDef::default()
            },
        );

        let user_config = Config::default();
        let root = PathBuf::from("/test");

        // Should not warn — settings-only overrides are valid.
        warn_orphan_project_servers(&project, &user_config, &root);
    }

    #[test]
    fn test_orphan_server_referenced_by_project_language() {
        // Server is referenced by project's own language config — not orphan.
        let mut project = ProjectConfig::default();
        project.server.insert(
            "my-server".to_string(),
            ServerDef {
                command: "my-server-bin".to_string(),
                args: Vec::new(),
                ..ServerDef::default()
            },
        );
        project.language.insert(
            "custom".to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new("my-server")]),
                ..LanguageConfig::default()
            },
        );

        let user_config = Config::default();
        let root = PathBuf::from("/test");

        warn_orphan_project_servers(&project, &user_config, &root);
    }

    #[test]
    fn test_orphan_server_referenced_by_user_language() {
        // Server is referenced by user's language config — not orphan.
        let mut project = ProjectConfig::default();
        project.server.insert(
            "rust-analyzer".to_string(),
            ServerDef {
                command: "custom-ra".to_string(),
                args: Vec::new(),
                ..ServerDef::default()
            },
        );

        let mut user_config = Config::default();
        let mut language = HashMap::new();
        language.insert(
            "rust".to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new("rust-analyzer")]),
                ..LanguageConfig::default()
            },
        );
        user_config.language = language;
        let root = PathBuf::from("/test");

        warn_orphan_project_servers(&project, &user_config, &root);
    }

    #[test]
    fn test_root_markers_empty_string_rejected() {
        let mut config = Config::default();
        config.language.insert(
            "test".to_string(),
            LanguageConfig {
                root_markers: Some(vec![String::new()]),
                ..LanguageConfig::default()
            },
        );

        let errors = validate(&config);
        assert!(
            errors.iter().any(|e| e.contains("root_markers")),
            "should reject empty string in root_markers: {errors:?}",
        );
    }

    #[test]
    fn test_root_markers_valid_entries_ok() {
        let mut config = Config::default();
        config.language.insert(
            "test".to_string(),
            LanguageConfig {
                extensions: Some(vec!["test".to_string()]),
                root_markers: Some(vec!["Cargo.toml".to_string()]),
                ..LanguageConfig::default()
            },
        );

        let errors = validate(&config);
        assert!(
            errors.is_empty(),
            "valid root_markers should pass: {errors:?}"
        );
    }

    #[test]
    fn test_root_markers_valid_glob_ok() {
        let mut config = Config::default();
        config.language.insert(
            "test".to_string(),
            LanguageConfig {
                extensions: Some(vec!["cs".to_string()]),
                root_markers: Some(vec!["*.sln".to_string(), "*.csproj".to_string()]),
                ..LanguageConfig::default()
            },
        );

        let errors = validate(&config);
        assert!(
            errors.is_empty(),
            "valid glob root_markers should pass: {errors:?}"
        );
    }

    #[test]
    fn test_root_markers_invalid_glob_rejected() {
        let mut config = Config::default();
        config.language.insert(
            "test".to_string(),
            LanguageConfig {
                extensions: Some(vec!["cs".to_string()]),
                root_markers: Some(vec!["[invalid".to_string()]),
                ..LanguageConfig::default()
            },
        );

        let errors = validate(&config);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("root_markers") && e.contains("[invalid")),
            "should reject invalid glob in root_markers: {errors:?}",
        );
    }

    #[test]
    fn test_root_markers_mixed_exact_and_glob_ok() {
        let mut config = Config::default();
        config.language.insert(
            "test".to_string(),
            LanguageConfig {
                extensions: Some(vec!["test".to_string()]),
                root_markers: Some(vec!["Cargo.toml".to_string(), "*.sln".to_string()]),
                ..LanguageConfig::default()
            },
        );

        let errors = validate(&config);
        assert!(
            errors.is_empty(),
            "mixed exact + glob root_markers should pass: {errors:?}"
        );
    }
}
