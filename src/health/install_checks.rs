// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Host-integration health checks: installed-hook staleness, agent-instruction
//! staleness, the `$PATH` binary, the legacy `constrained_bash.py` script, and
//! the resolved command-filter status.
//!
//! Each function returns typed [`Finding`]s. Staleness findings carry a
//! [`StaleDiff`] so a renderer can show a unified diff on demand.

use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::health::{Finding, FindingCode, Severity, StaleDiff};

/// Expected Claude Code hooks, embedded at compile time.
const CLAUDE_HOOKS_EXPECTED: &str = include_str!("../../plugins/catenary/hooks/hooks.json");

/// Expected Antigravity CLI hooks, embedded at compile time.
const ANTIGRAVITY_HOOKS_EXPECTED: &str =
    include_str!("../../plugins/catenary-antigravity/hooks.json");

/// Expected Antigravity rules file, embedded at compile time.
const ANTIGRAVITY_RULES_EXPECTED: &str =
    include_str!("../../plugins/catenary-antigravity/rules/catenary.md");

/// Migration guidance for users who still have the legacy Python script configured.
const CONSTRAINED_BASH_MIGRATION: &str = "Command filtering is now built into `catenary hook pre-tool`. \
     Remove the constrained_bash.py hook from your settings and use \
     `[commands]` in your Catenary config instead. \
     Run `catenary config` to generate a recommended template.";

/// Claude Code plugin-hook findings, compared against the shipped hooks.
#[must_use]
pub fn claude_hooks_findings() -> Vec<Finding> {
    let Ok(home_str) = std::env::var("HOME") else {
        return vec![Finding::new(
            FindingCode::NotInstalled,
            Severity::Info,
            "Claude Code hooks: cannot determine home directory",
        )];
    };
    claude_hooks_findings_at(&PathBuf::from(home_str))
}

/// [`claude_hooks_findings`] against an explicit home directory.
///
/// Split out so tests can exercise the stale/ok branches against a temp
/// layout without mutating `HOME` (env mutation is `unsafe` in edition 2024).
fn claude_hooks_findings_at(home: &Path) -> Vec<Finding> {
    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(plugins_json) = std::fs::read_to_string(&plugins_file) else {
        return vec![not_installed("Claude Code hooks")];
    };
    let Ok(plugins) = serde_json::from_str::<serde_json::Value>(&plugins_json) else {
        return vec![Finding::new(
            FindingCode::HooksUnreadable,
            Severity::Warning,
            "Claude Code hooks: cannot parse installed_plugins.json",
        )];
    };

    let entries = match plugins
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)
    {
        Some(arr) if !arr.is_empty() => arr,
        _ => return vec![not_installed("Claude Code hooks")],
    };

    let entry = &entries[0];
    let version = entry
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let Some(install_path_str) = entry.get("installPath").and_then(serde_json::Value::as_str)
    else {
        return vec![Finding::new(
            FindingCode::HooksUnreadable,
            Severity::Warning,
            format!("Claude Code hooks (v{version}): missing installPath"),
        )];
    };
    let install_path = PathBuf::from(install_path_str);

    let source_type = read_marketplace_source(home);
    let version_display = source_type
        .as_deref()
        .map_or_else(|| version.to_string(), |src| format!("{version} ({src})"));

    let hooks_path = install_path.join("hooks/hooks.json");
    let Ok(installed) = std::fs::read_to_string(&hooks_path) else {
        return vec![Finding::new(
            FindingCode::HooksMissing,
            Severity::Error,
            "Claude Code hooks.json not found in plugin cache",
        )];
    };

    if normalize_json(&installed) == normalize_json(CLAUDE_HOOKS_EXPECTED) {
        vec![Finding::new(
            FindingCode::HooksOk,
            Severity::Ok,
            format!("Claude Code hooks match (v{version_display})"),
        )]
    } else {
        vec![
            Finding::new(
                FindingCode::HooksStale,
                Severity::Error,
                format!("Claude Code hooks are stale (v{version_display})"),
            )
            .with_fix_it(
                // Not the bare `claude plugin uninstall && install` sequence:
                // Claude Code caches marketplace content by plugin version, so
                // for an unchanged version a bare reinstall re-copies the OLD
                // hooks. `catenary install claude` refreshes the marketplace
                // first (see `cli::install::claude_ensure_marketplace`).
                "Run: catenary install claude (refreshes the marketplace cache, \
                 then reinstalls — a bare plugin reinstall can re-copy the stale \
                 cached content)",
            )
            .with_diff(StaleDiff {
                installed: pretty_json(&installed),
                expected: pretty_json(CLAUDE_HOOKS_EXPECTED),
            }),
        ]
    }
}

/// Antigravity CLI plugin-hook findings, compared against the shipped hooks.
///
/// Searches workspace paths (relative to `project_root`) then the global path.
#[must_use]
pub fn antigravity_hooks_findings(project_root: &Path) -> Vec<Finding> {
    let Some((plugin_dir, scope)) = find_antigravity_plugin_dir(project_root) else {
        return vec![not_installed("Antigravity hooks")];
    };

    let hooks_path = plugin_dir.join("hooks.json");
    let Ok(installed) = std::fs::read_to_string(&hooks_path) else {
        return vec![Finding::new(
            FindingCode::HooksUnreadable,
            Severity::Warning,
            format!("Antigravity hooks.json not found ({scope})"),
        )];
    };

    if normalize_json(&installed) == normalize_json(ANTIGRAVITY_HOOKS_EXPECTED) {
        vec![Finding::new(
            FindingCode::HooksOk,
            Severity::Ok,
            format!("Antigravity hooks match ({scope})"),
        )]
    } else {
        vec![
            Finding::new(
                FindingCode::HooksStale,
                Severity::Error,
                format!("Antigravity hooks are stale ({scope})"),
            )
            .with_fix_it("Reinstall the plugin: catenary install antigravity")
            .with_diff(StaleDiff {
                installed: pretty_json(&installed),
                expected: pretty_json(ANTIGRAVITY_HOOKS_EXPECTED),
            }),
        ]
    }
}

/// Whether the running binary matches what `$PATH` would resolve.
#[must_use]
pub fn path_binary_findings() -> Vec<Finding> {
    let Some(current_exe) = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok())
    else {
        return vec![Finding::new(
            FindingCode::PathMismatch,
            Severity::Warning,
            "PATH: cannot determine current executable",
        )];
    };

    let path_var = std::env::var("PATH").unwrap_or_default();
    let Some(path_binary) = std::env::split_paths(&path_var)
        .map(|dir| dir.join("catenary"))
        .find(|p| p.is_file())
    else {
        return vec![Finding::new(
            FindingCode::PathMismatch,
            Severity::Error,
            "PATH: catenary not found on PATH",
        )];
    };

    let resolved_path = std::fs::canonicalize(&path_binary).unwrap_or(path_binary);
    if current_exe == resolved_path {
        vec![Finding::new(
            FindingCode::PathOk,
            Severity::Ok,
            format!("PATH: {}", resolved_path.display()),
        )]
    } else {
        vec![Finding::new(
            FindingCode::PathMismatch,
            Severity::Error,
            format!(
                "PATH: {} differs from {}",
                resolved_path.display(),
                current_exe.display(),
            ),
        )]
    }
}

/// Claude Code plugin registration staleness (installed version vs this build).
#[must_use]
pub fn claude_instructions_findings() -> Vec<Finding> {
    let Ok(home_str) = std::env::var("HOME") else {
        return vec![Finding::new(
            FindingCode::NotInstalled,
            Severity::Info,
            "Claude Code instructions: cannot determine home directory",
        )];
    };
    let home = PathBuf::from(home_str);

    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(plugins_json) = std::fs::read_to_string(&plugins_file) else {
        return vec![not_installed("Claude Code instructions")];
    };
    let Ok(plugins) = serde_json::from_str::<serde_json::Value>(&plugins_json) else {
        return vec![Finding::new(
            FindingCode::HooksUnreadable,
            Severity::Warning,
            "Claude Code instructions: cannot parse installed_plugins.json",
        )];
    };

    let entries = match plugins
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)
    {
        Some(arr) if !arr.is_empty() => arr,
        _ => return vec![not_installed("Claude Code instructions")],
    };

    let entry = &entries[0];
    let installed_version = entry
        .get("version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    // Compare against CARGO_PKG_VERSION (the semver marketplace.json carries),
    // not CATENARY_VERSION which adds git-describe commit distance on dev builds.
    let expected_version = env!("CARGO_PKG_VERSION");

    if entry
        .get("installPath")
        .and_then(serde_json::Value::as_str)
        .is_none()
    {
        return vec![Finding::new(
            FindingCode::HooksUnreadable,
            Severity::Warning,
            "Claude Code instructions: missing installPath",
        )];
    }

    if installed_version == expected_version {
        vec![Finding::new(
            FindingCode::InstructionsOk,
            Severity::Ok,
            format!("Claude Code up to date (v{installed_version})"),
        )]
    } else {
        vec![
            Finding::new(
                FindingCode::InstructionsStale,
                Severity::Error,
                format!(
                    "Claude Code plugin is stale (v{installed_version} installed, \
                     v{expected_version} expected)"
                ),
            )
            .with_fix_it("Run: catenary install claude"),
        ]
    }
}

/// Antigravity CLI agent-instruction (rules) staleness.
///
/// Symlinked installs are current by definition; a valid runtime generation
/// stamp reads as current too (teaching-surface 12).
#[must_use]
pub fn antigravity_instructions_findings(project_root: &Path) -> Vec<Finding> {
    let Some((plugin_dir, _scope)) = find_antigravity_plugin_dir(project_root) else {
        return vec![not_installed("Antigravity instructions")];
    };

    if plugin_dir.is_symlink() {
        return vec![Finding::new(
            FindingCode::InstructionsOk,
            Severity::Ok,
            "Antigravity rules symlinked (always current)",
        )];
    }

    let rules_path = plugin_dir.join("rules/catenary.md");
    let Ok(content) = std::fs::read_to_string(&rules_path) else {
        return vec![
            Finding::new(
                FindingCode::InstructionsStale,
                Severity::Error,
                "Antigravity rules/catenary.md not found",
            )
            .with_fix_it("Run: catenary install antigravity"),
        ];
    };

    if content == ANTIGRAVITY_RULES_EXPECTED {
        vec![Finding::new(
            FindingCode::InstructionsOk,
            Severity::Ok,
            "Antigravity rules up to date",
        )]
    } else if crate::cli::teaching::is_runtime_stamped(&content) {
        vec![Finding::new(
            FindingCode::InstructionsOk,
            Severity::Ok,
            "Antigravity rules up to date (runtime-updated)",
        )]
    } else {
        vec![
            Finding::new(
                FindingCode::InstructionsStale,
                Severity::Error,
                "Antigravity rules are stale",
            )
            .with_fix_it("Run: catenary install antigravity")
            .with_diff(StaleDiff {
                installed: content,
                expected: ANTIGRAVITY_RULES_EXPECTED.to_string(),
            }),
        ]
    }
}

/// Legacy `constrained_bash.py` findings for Claude Code's settings.
#[must_use]
pub fn legacy_script_findings() -> Vec<Finding> {
    let Ok(home_str) = std::env::var("HOME") else {
        return Vec::new();
    };
    let settings_path = PathBuf::from(home_str).join(".claude/settings.json");
    let Ok(settings_json) = std::fs::read_to_string(&settings_path) else {
        return Vec::new();
    };
    let Ok(settings) = serde_json::from_str::<serde_json::Value>(&settings_json) else {
        return Vec::new();
    };

    if find_script_path_in_json(&settings, "constrained_bash.py").is_some() {
        vec![
            Finding::new(
                FindingCode::LegacyScript,
                Severity::Warning,
                "Claude Code: legacy constrained_bash.py detected",
            )
            .with_fix_it(CONSTRAINED_BASH_MIGRATION),
        ]
    } else {
        Vec::new()
    }
}

/// The resolved command-filter status, as a single informational finding.
#[must_use]
pub fn command_filter_findings(config: &Config) -> Vec<Finding> {
    let finding = match &config.resolved_commands {
        Some(resolved) if resolved.client_enforcement_only => Finding::new(
            FindingCode::CommandFilterStatus,
            Severity::Info,
            "client_enforcement_only — Catenary enforcement disabled",
        ),
        Some(resolved) if resolved.is_active() => {
            let total = resolved.allow.len() + resolved.pipeline.len();
            let build_suffix = build_suffix(resolved);
            Finding::new(
                FindingCode::CommandFilterStatus,
                Severity::Ok,
                format!(
                    "{total} command{} allowed{build_suffix}",
                    if total == 1 { "" } else { "s" },
                ),
            )
        }
        Some(_) | None => Finding::new(
            FindingCode::CommandFilterStatus,
            Severity::Info,
            "no [commands] section — all shell commands allowed",
        ),
    };
    vec![finding]
}

/// Build the `, build tool(s): ...` suffix for the active command-filter line.
fn build_suffix(resolved: &crate::config::ResolvedCommands) -> String {
    if !resolved.default_build.is_empty() {
        let tools = resolved.default_build.join(", ");
        format!(
            ", build tool{}: {tools}",
            if resolved.default_build.len() == 1 {
                ""
            } else {
                "s"
            },
        )
    } else if !resolved.build.is_empty() {
        let mut tools: Vec<&str> = resolved
            .build
            .values()
            .flat_map(|v| v.iter().map(String::as_str))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        tools.sort_unstable();
        format!(
            ", build tool{}: {}",
            if tools.len() == 1 { "" } else { "s" },
            tools.join(", ")
        )
    } else {
        String::new()
    }
}

/// A "not installed" info finding for `label`.
fn not_installed(label: &str) -> Finding {
    Finding::new(
        FindingCode::NotInstalled,
        Severity::Info,
        format!("{label}: not installed"),
    )
}

/// Discover the Antigravity plugin directory: workspace paths (relative to
/// `project_root`) first, then the global path. Returns the directory and a
/// scope label (`workspace` / `global`).
fn find_antigravity_plugin_dir(project_root: &Path) -> Option<(PathBuf, &'static str)> {
    let resolved_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());

    let workspace_candidates = [
        resolved_root.join(".agents/plugins/catenary"),
        resolved_root.join("_agents/plugins/catenary"),
    ];
    if let Some(dir) = workspace_candidates.into_iter().find(|p| p.is_dir()) {
        return Some((dir, "workspace"));
    }

    let global_candidate = std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".gemini/config/plugins/catenary"))
        .filter(|p| p.is_dir());
    global_candidate.map(|p| (p, "global"))
}

/// Read the catenary marketplace source type from `known_marketplaces.json`.
fn read_marketplace_source(home: &Path) -> Option<String> {
    let path = home.join(".claude/plugins/known_marketplaces.json");
    let contents = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&contents).ok()?;
    json.get("catenary")
        .and_then(|c| c.get("source"))
        .and_then(|s| s.get("source"))
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string)
}

/// Normalize a JSON string for comparison (parse and re-serialize).
fn normalize_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string(&v).ok())
        .unwrap_or_else(|| s.trim().to_string())
}

/// Pretty-print a JSON string for use in human-readable diffs.
fn pretty_json(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_string())
}

/// Walk all string values in `json` and return the whitespace-split token that
/// contains `needle`, searching depth-first.
fn find_script_path_in_json(json: &serde_json::Value, needle: &str) -> Option<String> {
    match json {
        serde_json::Value::String(s) if s.contains(needle) => s
            .split_whitespace()
            .find(|token| token.contains(needle))
            .map(std::string::ToString::to_string),
        serde_json::Value::Object(map) => map
            .values()
            .find_map(|v| find_script_path_in_json(v, needle)),
        serde_json::Value::Array(arr) => {
            arr.iter().find_map(|v| find_script_path_in_json(v, needle))
        }
        _ => None,
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

    #[test]
    fn embedded_antigravity_rules_non_empty() {
        assert!(!ANTIGRAVITY_RULES_EXPECTED.trim().is_empty());
    }

    // ── antigravity instructions (teaching-surface 12) ──────────────

    #[test]
    fn antigravity_instructions_accepts_runtime_stamped_rules() {
        let root = tempfile::tempdir().expect("tempdir");
        let plugin_dir = root.path().join(".agents/plugins/catenary/rules");
        fs::create_dir_all(&plugin_dir).expect("create workspace plugin dir");
        fs::write(
            plugin_dir.join("catenary.md"),
            crate::cli::teaching::antigravity_rules_file(),
        )
        .expect("write runtime-stamped rules");

        let findings = antigravity_instructions_findings(root.path());
        let finding = findings.first().expect("one finding");
        assert_eq!(finding.code, FindingCode::InstructionsOk);
        assert!(
            finding.message.contains("runtime-updated"),
            "runtime-stamped rules read as current: {}",
            finding.message,
        );
    }

    #[test]
    fn antigravity_instructions_flags_unstamped_divergent_rules() {
        let root = tempfile::tempdir().expect("tempdir");
        let plugin_dir = root.path().join(".agents/plugins/catenary/rules");
        fs::create_dir_all(&plugin_dir).expect("create workspace plugin dir");
        fs::write(plugin_dir.join("catenary.md"), "handwritten drift\n").expect("write drift");

        let findings = antigravity_instructions_findings(root.path());
        let finding = findings.first().expect("one finding");
        assert_eq!(finding.code, FindingCode::InstructionsStale);
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.diff.is_some(), "stale rules carry a diff");
    }

    // ── claude hooks staleness ──────────────────────────────────────

    /// Lay out a fake `~/.claude/plugins` install: a version-keyed cache dir
    /// holding `hooks_content` and an `installed_plugins.json` pointing at it.
    fn write_claude_plugin_layout(home: &Path, hooks_content: &str) {
        let cache = home.join(".claude/plugins/cache/catenary/catenary/1.6.1");
        fs::create_dir_all(cache.join("hooks")).expect("create plugin cache dir");
        fs::write(cache.join("hooks/hooks.json"), hooks_content).expect("write cached hooks");
        let installed = serde_json::json!({
            "version": 2,
            "plugins": {
                "catenary@catenary": [{
                    "scope": "user",
                    "installPath": cache.to_string_lossy(),
                    "version": "1.6.1",
                }],
            },
        });
        fs::write(
            home.join(".claude/plugins/installed_plugins.json"),
            serde_json::to_string_pretty(&installed).expect("serialize installed_plugins"),
        )
        .expect("write installed_plugins.json");
    }

    #[test]
    fn claude_hooks_matching_cache_reads_ok() {
        let home = tempfile::tempdir().expect("tempdir");
        write_claude_plugin_layout(home.path(), CLAUDE_HOOKS_EXPECTED);

        let findings = claude_hooks_findings_at(home.path());
        let finding = findings.first().expect("one finding");
        assert_eq!(finding.code, FindingCode::HooksOk);
    }

    #[test]
    fn claude_hooks_stale_fix_it_names_catenary_install() {
        // The version-keyed plugin cache freezes hooks at install time; when
        // the shipped set moves on, the fix-it must name `catenary install
        // claude` — which refreshes the marketplace before reinstalling — and
        // NOT the bare `claude plugin uninstall && install` sequence, which
        // re-copies the stale version-cached marketplace content (see
        // `cli::install::claude_ensure_marketplace`).
        let home = tempfile::tempdir().expect("tempdir");
        write_claude_plugin_layout(home.path(), r#"{"hooks":{}}"#);

        let findings = claude_hooks_findings_at(home.path());
        let finding = findings.first().expect("one finding");
        assert_eq!(finding.code, FindingCode::HooksStale);
        assert_eq!(finding.severity, Severity::Error);
        assert!(finding.diff.is_some(), "stale hooks carry a diff");
        let fix_it = finding.fix_it.as_deref().expect("stale hooks carry fix-it");
        assert!(
            fix_it.contains("catenary install claude"),
            "fix-it names the marketplace-refreshing reinstall: {fix_it}",
        );
        assert!(
            !fix_it.starts_with("Reinstall: claude plugin uninstall"),
            "fix-it must not lead with the bare reinstall known to re-copy \
             stale cached content: {fix_it}",
        );
    }

    // ── command filter status ───────────────────────────────────────

    fn filter_finding(resolved: Option<crate::config::ResolvedCommands>) -> Finding {
        let config = Config {
            resolved_commands: resolved,
            ..Default::default()
        };
        command_filter_findings(&config)
            .into_iter()
            .next()
            .expect("one command-filter finding")
    }

    #[test]
    fn command_filter_client_enforcement_only() {
        let resolved = crate::config::ResolvedCommands {
            client_enforcement_only: true,
            allow: std::iter::once("git".to_string()).collect(),
            ..Default::default()
        };
        let finding = filter_finding(Some(resolved));
        assert_eq!(finding.severity, Severity::Info);
        assert!(
            finding.message.contains("client_enforcement_only"),
            "got: {}",
            finding.message,
        );
    }

    #[test]
    fn command_filter_active_count_and_arithmetic() {
        let resolved = crate::config::ResolvedCommands {
            allow: ["git", "cat", "ls"].into_iter().map(String::from).collect(),
            pipeline: ["grep", "head"].into_iter().map(String::from).collect(),
            ..Default::default()
        };
        let finding = filter_finding(Some(resolved));
        assert_eq!(finding.severity, Severity::Ok);
        assert!(
            finding.message.contains("5 commands allowed"),
            "allow=3 + pipeline=2 → 5: {}",
            finding.message,
        );
    }

    #[test]
    fn command_filter_singular_command() {
        let resolved = crate::config::ResolvedCommands {
            allow: std::iter::once("git".to_string()).collect(),
            ..Default::default()
        };
        let finding = filter_finding(Some(resolved));
        assert!(
            finding.message.contains("1 command allowed"),
            "got: {}",
            finding.message,
        );
    }

    #[test]
    fn command_filter_inactive_and_none_render_no_section() {
        let inactive = filter_finding(Some(crate::config::ResolvedCommands::default()));
        assert_eq!(inactive.severity, Severity::Info);
        assert!(inactive.message.contains("no [commands] section"));

        let none = filter_finding(None);
        assert!(none.message.contains("no [commands] section"));
    }

    #[test]
    fn command_filter_default_build_suffix() {
        let resolved = crate::config::ResolvedCommands {
            allow: std::iter::once("git".to_string()).collect(),
            default_build: vec!["make".to_string()],
            ..Default::default()
        };
        let finding = filter_finding(Some(resolved));
        assert!(
            finding.message.contains("build tool: make"),
            "got: {}",
            finding.message,
        );
    }

    #[test]
    fn command_filter_per_root_build_suffix() {
        let build = std::iter::once((PathBuf::from("/repo"), vec!["cargo".to_string()])).collect();
        let resolved = crate::config::ResolvedCommands {
            allow: std::iter::once("git".to_string()).collect(),
            build,
            ..Default::default()
        };
        let finding = filter_finding(Some(resolved));
        assert!(
            finding.message.contains("build tool: cargo"),
            "got: {}",
            finding.message,
        );
    }

    // ── json helpers ────────────────────────────────────────────────

    #[test]
    fn normalize_json_canonicalizes() {
        let result = normalize_json(r#"{ "b": 2, "a": 1 }"#);
        assert!(serde_json::from_str::<serde_json::Value>(&result).is_ok());
    }

    #[test]
    fn pretty_json_formats_readably() {
        assert!(pretty_json(r#"{"a":1,"b":2}"#).contains('\n'));
    }
}
