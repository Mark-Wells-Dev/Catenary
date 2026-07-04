// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Install command: install or update the Catenary plugin for a host CLI.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

use crate::cli::Output;

/// Source for installation: local dev path or remote repo identifier.
#[derive(Debug, Clone)]
enum InstallSource {
    /// Local filesystem path (dev install via link/symlink).
    Local(PathBuf),
    /// Remote repository identifier (release install).
    Remote(String),
}

/// Parse a source string into an [`InstallSource`].
///
/// A path starting with `/`, `./`, or `~` is treated as local.
/// Everything else is a repo identifier.
fn parse_source(source: &str) -> InstallSource {
    if source.starts_with('/') || source.starts_with("./") || source.starts_with('~') {
        let expanded = source.strip_prefix('~').map_or_else(
            || PathBuf::from(source),
            |rest| {
                dirs::home_dir().map_or_else(
                    || PathBuf::from(source),
                    |h| h.join(rest.strip_prefix('/').unwrap_or(rest)),
                )
            },
        );
        InstallSource::Local(expanded)
    } else {
        InstallSource::Remote(source.to_string())
    }
}

/// Default repo identifier when no source is specified.
const DEFAULT_REPO: &str = "TwoWells/Catenary";

/// Claude Code marketplace name for the catenary plugin (the `@catenary` in
/// `catenary@catenary`). Used to refresh the marketplace's cached content.
const CLAUDE_MARKETPLACE: &str = "catenary";

// ── Host detection ─────────────────────────────────────────────────

/// Check whether a binary can be found on `$PATH`.
fn binary_exists(command: &str) -> bool {
    let path_var = std::env::var("PATH").unwrap_or_default();
    std::env::split_paths(&path_var)
        .map(|dir| dir.join(command))
        .any(|p| p.is_file())
}

/// Detected host with its install status.
struct HostStatus {
    name: &'static str,
    detected: bool,
    status: String,
}

/// Detect all hosts and their install status.
fn detect_hosts() -> Vec<HostStatus> {
    vec![
        detect_claude(),
        detect_gemini(),
        detect_antigravity(),
        detect_opencode(),
    ]
}

/// Detect Claude Code install status.
fn detect_claude() -> HostStatus {
    let detected = binary_exists("claude");
    let status = if detected {
        claude_install_status()
    } else {
        "not detected".to_string()
    };
    HostStatus {
        name: "claude",
        detected,
        status,
    }
}

/// Get Claude Code plugin install status from `installed_plugins.json`.
fn claude_install_status() -> String {
    let Some(home) = dirs::home_dir() else {
        return "unknown".to_string();
    };
    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(json_str) = std::fs::read_to_string(&plugins_file) else {
        return "not installed".to_string();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) else {
        return "not installed".to_string();
    };

    let entries = json
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array);

    match entries {
        Some(arr) if !arr.is_empty() => {
            let version = arr[0]
                .get("version")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            format!("installed (v{version})")
        }
        _ => "not installed".to_string(),
    }
}

/// Detect Gemini CLI install status.
fn detect_gemini() -> HostStatus {
    let detected = binary_exists("gemini");
    let status = if detected {
        gemini_install_status()
    } else {
        "not detected".to_string()
    };
    HostStatus {
        name: "gemini",
        detected,
        status,
    }
}

/// Get Gemini CLI extension install status.
fn gemini_install_status() -> String {
    let Some(home) = dirs::home_dir() else {
        return "unknown".to_string();
    };
    let ext_dir = home.join(".gemini/extensions");
    let ext_path = ["Catenary", "catenary"]
        .iter()
        .map(|name| ext_dir.join(name))
        .find(|p| p.is_dir());

    let Some(ext_path) = ext_path else {
        return "not installed".to_string();
    };

    // Check install type
    let install_meta_path = ext_path.join(".gemini-extension-install.json");
    let install_meta = std::fs::read_to_string(&install_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let install_type = install_meta
        .as_ref()
        .and_then(|m| m.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown");

    // Resolve manifest path for version
    let resolved = if install_type == "link" {
        install_meta
            .as_ref()
            .and_then(|m| m.get("source").and_then(serde_json::Value::as_str))
            .map_or_else(|| ext_path.clone(), PathBuf::from)
    } else {
        ext_path
    };

    let manifest_path = resolved.join("gemini-extension.json");
    let version = std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("version")
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string)
        });

    let type_label = if install_type == "link" {
        "linked"
    } else {
        "installed"
    };

    version.map_or_else(
        || type_label.to_string(),
        |v| format!("{type_label} (v{v})"),
    )
}

/// Detect Antigravity CLI install status.
fn detect_antigravity() -> HostStatus {
    let agy_exists = binary_exists("agy");
    let global_dir_exists =
        dirs::home_dir().is_some_and(|h| h.join(".gemini/config/plugins/catenary").is_dir());
    let detected = agy_exists || global_dir_exists;

    let status = if !detected {
        "not detected".to_string()
    } else if global_dir_exists {
        "installed (global)".to_string()
    } else {
        "not installed".to_string()
    };

    HostStatus {
        name: "antigravity",
        detected,
        status,
    }
}

/// Detect OpenCode install status.
fn detect_opencode() -> HostStatus {
    let binary = binary_exists("opencode");
    let plugin = dirs::home_dir().map(|h| h.join(".config/opencode/plugin/catenary.js"));
    let linked = plugin.as_ref().is_some_and(|p| p.is_symlink());
    // `is_file()` follows symlinks, so check `linked` first to distinguish.
    let bundled = plugin
        .as_ref()
        .is_some_and(|p| !p.is_symlink() && p.is_file());
    let detected = binary || linked || bundled;

    let status = if !detected {
        "not detected".to_string()
    } else if linked {
        "linked (global)".to_string()
    } else if bundled {
        "installed (global)".to_string()
    } else {
        "not installed".to_string()
    };

    HostStatus {
        name: "opencode",
        detected,
        status,
    }
}

// ── List subcommand ────────────────────────────────────────────────

/// Run the bare `catenary install` command: list detected hosts.
///
/// # Errors
///
/// Returns an I/O error if output fails.
pub fn run_install_list(out: &mut Output) -> Result<()> {
    let _ = out.writeln(format_args!("Detected hosts:"));

    let hosts = detect_hosts();
    for host in &hosts {
        let name_col = format!("  {:<14}", host.name);
        let status = if host.detected {
            out.colors.green(&host.status)
        } else {
            out.colors.dim(&host.status)
        };
        let _ = out.writeln(format_args!("{name_col}{status}"));
    }

    Ok(())
}

// ── Claude Code install ────────────────────────────────────────────

/// Run `catenary install claude`.
///
/// # Errors
///
/// Returns an error if the Claude Code binary is not found or
/// plugin commands fail.
pub fn run_install_claude(out: &mut Output, source: Option<&str>, dry_run: bool) -> Result<()> {
    let _ = out.writeln(format_args!("Claude Code:"));

    if !binary_exists("claude") {
        let _ = out.writeln(format_args!(
            "  {}",
            out.colors.red("✗ `claude` not found on PATH"),
        ));
        return Ok(());
    }

    let parsed_source = source.map(parse_source);

    // Determine marketplace source to use
    let marketplace_source = match &parsed_source {
        Some(InstallSource::Local(path)) => path.display().to_string(),
        Some(InstallSource::Remote(repo)) => repo.clone(),
        None => read_claude_marketplace_source().unwrap_or_else(|| DEFAULT_REPO.to_string()),
    };

    // Step 1: Ensure marketplace is registered
    let needs_marketplace_update =
        parsed_source.is_some() || read_claude_marketplace_source().is_none();

    if !claude_ensure_marketplace(out, &marketplace_source, needs_marketplace_update, dry_run)? {
        return Ok(());
    }

    // Step 2: Uninstall existing plugin (if any) and reinstall
    claude_ensure_plugin(out, dry_run)
}

/// Ensure the Claude Code marketplace is registered AND its content is fresh.
///
/// Registration (`marketplace add`) only runs for a new or changed source. The
/// content refresh (`marketplace update`) runs ALWAYS, because a marketplace that
/// is already registered with the right source can still serve a STALE snapshot:
/// for a `directory` source whose plugin version is unchanged, Claude caches the
/// marketplace content by version, so a plugin reinstall would re-copy the OLD
/// hooks (the "stale hooks persist after `catenary install`" gap). `update`
/// re-reads the source so the subsequent reinstall picks up current hooks. The
/// refresh is non-fatal — a possibly-stale reinstall beats a blocked install.
///
/// Returns `Ok(true)` if the caller should proceed to plugin install,
/// `Ok(false)` if registration failed.
fn claude_ensure_marketplace(
    out: &mut Output,
    source: &str,
    needs_update: bool,
    dry_run: bool,
) -> Result<bool> {
    // Register the marketplace (only when the source is new or changed).
    if needs_update {
        if dry_run {
            let _ = out.writeln(format_args!(
                "  {} marketplace add {source}",
                out.colors.dim("(dry-run)"),
            ));
        } else {
            let status = Command::new("claude")
                .args(["plugin", "marketplace", "add", source])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .status()
                .context("failed to run `claude plugin marketplace add`")?;
            if status.success() {
                let _ = out.writeln(format_args!(
                    "  {} marketplace registered ({source})",
                    out.colors.green("✓"),
                ));
            } else {
                let _ = out.writeln(format_args!(
                    "  {} marketplace add failed",
                    out.colors.red("✗"),
                ));
                return Ok(false);
            }
        }
    } else {
        let _ = out.writeln(format_args!(
            "  {} marketplace registered ({source})",
            out.colors.green("✓"),
        ));
    }

    // Refresh the marketplace content so the reinstall below picks up the current
    // hooks even when the plugin version is unchanged (directory marketplaces /
    // dev installs). Without this, a registered-but-cached marketplace re-serves
    // stale hooks and the daemon keeps emitting "Stale … hooks detected".
    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} marketplace update {CLAUDE_MARKETPLACE}",
            out.colors.dim("(dry-run)"),
        ));
    } else {
        match Command::new("claude")
            .args(["plugin", "marketplace", "update", CLAUDE_MARKETPLACE])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .status()
        {
            Ok(status) if status.success() => {
                let _ = out.writeln(format_args!(
                    "  {} marketplace refreshed",
                    out.colors.green("✓"),
                ));
            }
            _ => {
                let _ = out.writeln(format_args!(
                    "  {} marketplace update failed (reinstall may use cached content)",
                    out.colors.yellow("⚠"),
                ));
            }
        }
    }

    Ok(true)
}

/// Ensure the Claude Code plugin is installed (uninstall + reinstall cycle).
fn claude_ensure_plugin(out: &mut Output, dry_run: bool) -> Result<()> {
    let is_installed = claude_plugin_is_installed();

    if is_installed {
        if dry_run {
            let _ = out.writeln(format_args!(
                "  {} plugin uninstall catenary && plugin install catenary@catenary",
                out.colors.dim("(dry-run)"),
            ));
            return Ok(());
        }

        let uninstall = Command::new("claude")
            .args(["plugin", "uninstall", "catenary"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .status()
            .context("failed to run `claude plugin uninstall`")?;

        if !uninstall.success() {
            let _ = out.writeln(format_args!(
                "  {} plugin uninstall failed",
                out.colors.red("✗"),
            ));
            return Ok(());
        }
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} plugin install catenary@catenary",
            out.colors.dim("(dry-run)"),
        ));
        return Ok(());
    }

    let install = Command::new("claude")
        .args(["plugin", "install", "catenary@catenary"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .status()
        .context("failed to run `claude plugin install`")?;

    if install.success() {
        let verb = if is_installed {
            "reinstalled"
        } else {
            "installed"
        };
        let _ = out.writeln(format_args!("  {} plugin {verb}", out.colors.green("✓")));
    } else {
        let _ = out.writeln(format_args!(
            "  {} plugin install failed",
            out.colors.red("✗"),
        ));
    }

    Ok(())
}

/// Check if the catenary plugin is currently installed in Claude Code.
fn claude_plugin_is_installed() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let Ok(json_str) = std::fs::read_to_string(&plugins_file) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) else {
        return false;
    };

    json.get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|arr| !arr.is_empty())
}

/// Read the current Claude Code marketplace source for catenary.
fn read_claude_marketplace_source() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home.join(".claude/plugins/known_marketplaces.json");
    let contents = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&contents).ok()?;
    json.get("catenary")
        .and_then(|c| c.get("source"))
        .and_then(|s| s.get("source"))
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string)
}

// ── Gemini CLI install ─────────────────────────────────────────────

/// Run `catenary install gemini`.
///
/// # Errors
///
/// Returns an error if the Gemini CLI binary is not found or
/// extension commands fail.
pub fn run_install_gemini(out: &mut Output, source: Option<&str>, dry_run: bool) -> Result<()> {
    let _ = out.writeln(format_args!("Gemini CLI:"));

    if !binary_exists("gemini") {
        let _ = out.writeln(format_args!(
            "  {}",
            out.colors.red("✗ `gemini` not found on PATH"),
        ));
        return Ok(());
    }

    match source.map(parse_source) {
        Some(InstallSource::Local(path)) => {
            install_gemini_local(out, &path, dry_run)?;
        }
        Some(InstallSource::Remote(repo)) => {
            install_gemini_remote(out, &repo, dry_run)?;
        }
        None => {
            install_gemini_refresh(out, dry_run)?;
        }
    }

    Ok(())
}

/// Install Gemini extension from a local path (link).
fn install_gemini_local(out: &mut Output, path: &Path, dry_run: bool) -> Result<()> {
    let current = gemini_current_install();

    // If already linked to same path, no-op
    if let Some((install_type, source_path)) = &current {
        if install_type == "link" && source_path.as_deref() == Some(&*path.to_string_lossy()) {
            let _ = out.writeln(format_args!(
                "  {} linked → {}",
                out.colors.green("✓"),
                path.display(),
            ));
            return Ok(());
        }

        // Different source — uninstall first
        if dry_run {
            let _ = out.writeln(format_args!(
                "  {} extensions uninstall catenary",
                out.colors.dim("(dry-run)"),
            ));
        } else {
            let _ = Command::new("gemini")
                .args(["extensions", "uninstall", "catenary"])
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .status();
        }
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} extensions link {}",
            out.colors.dim("(dry-run)"),
            path.display(),
        ));
    } else {
        let status = Command::new("gemini")
            .args(["extensions", "link", &path.to_string_lossy()])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .status()
            .context("failed to run `gemini extensions link`")?;

        if status.success() {
            let _ = out.writeln(format_args!(
                "  {} linked → {}",
                out.colors.green("✓"),
                path.display(),
            ));
        } else {
            let _ = out.writeln(format_args!(
                "  {} extensions link failed",
                out.colors.red("✗"),
            ));
        }
    }

    Ok(())
}

/// Install Gemini extension from a remote repo.
fn install_gemini_remote(out: &mut Output, repo: &str, dry_run: bool) -> Result<()> {
    let current = gemini_current_install();

    if let Some((install_type, _)) = &current {
        if install_type == "link" {
            // Linked — need to uninstall and install from remote
            if dry_run {
                let _ = out.writeln(format_args!(
                    "  {} extensions uninstall catenary && extensions install {repo}",
                    out.colors.dim("(dry-run)"),
                ));
            } else {
                let _ = Command::new("gemini")
                    .args(["extensions", "uninstall", "catenary"])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status();

                let status = Command::new("gemini")
                    .args(["extensions", "install", repo])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .context("failed to run `gemini extensions install`")?;

                if status.success() {
                    let _ = out.writeln(format_args!(
                        "  {} extension installed from {repo}",
                        out.colors.green("✓"),
                    ));
                } else {
                    let _ = out.writeln(format_args!(
                        "  {} extensions install failed",
                        out.colors.red("✗"),
                    ));
                }
            }
        } else {
            // Already installed from remote — update
            if dry_run {
                let _ = out.writeln(format_args!(
                    "  {} extensions update catenary",
                    out.colors.dim("(dry-run)"),
                ));
            } else {
                let status = Command::new("gemini")
                    .args(["extensions", "update", "catenary"])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .context("failed to run `gemini extensions update`")?;

                if status.success() {
                    let _ = out.writeln(format_args!(
                        "  {} extension updated",
                        out.colors.green("✓"),
                    ));
                } else {
                    let _ = out.writeln(format_args!(
                        "  {} extensions update failed",
                        out.colors.red("✗"),
                    ));
                }
            }
        }
    } else if dry_run {
        let _ = out.writeln(format_args!(
            "  {} extensions install {repo}",
            out.colors.dim("(dry-run)"),
        ));
    } else {
        let status = Command::new("gemini")
            .args(["extensions", "install", repo])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .status()
            .context("failed to run `gemini extensions install`")?;

        if status.success() {
            let _ = out.writeln(format_args!(
                "  {} extension installed from {repo}",
                out.colors.green("✓"),
            ));
        } else {
            let _ = out.writeln(format_args!(
                "  {} extensions install failed",
                out.colors.red("✗"),
            ));
        }
    }

    Ok(())
}

/// Refresh existing Gemini extension install (no source argument).
fn install_gemini_refresh(out: &mut Output, dry_run: bool) -> Result<()> {
    let current = gemini_current_install();

    match current {
        Some((install_type, source_path)) if install_type == "link" => {
            // Linked — always current, no-op
            let display = source_path.as_deref().unwrap_or("(unknown)");
            let _ = out.writeln(format_args!(
                "  {} linked → {display} (always current)",
                out.colors.green("✓"),
            ));
        }
        Some(_) => {
            // Installed from remote — update
            if dry_run {
                let _ = out.writeln(format_args!(
                    "  {} extensions update catenary",
                    out.colors.dim("(dry-run)"),
                ));
            } else {
                let status = Command::new("gemini")
                    .args(["extensions", "update", "catenary"])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .context("failed to run `gemini extensions update`")?;

                if status.success() {
                    let _ = out.writeln(format_args!(
                        "  {} extension updated",
                        out.colors.green("✓"),
                    ));
                } else {
                    let _ = out.writeln(format_args!(
                        "  {} extensions update failed",
                        out.colors.red("✗"),
                    ));
                }
            }
        }
        None => {
            // Not installed — install from default repo
            let repo = DEFAULT_REPO;
            if dry_run {
                let _ = out.writeln(format_args!(
                    "  {} extensions install {repo}",
                    out.colors.dim("(dry-run)"),
                ));
            } else {
                let status = Command::new("gemini")
                    .args(["extensions", "install", repo])
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .status()
                    .context("failed to run `gemini extensions install`")?;

                if status.success() {
                    let _ = out.writeln(format_args!(
                        "  {} extension installed from {repo}",
                        out.colors.green("✓"),
                    ));
                } else {
                    let _ = out.writeln(format_args!(
                        "  {} extensions install failed",
                        out.colors.red("✗"),
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Get current Gemini extension install info: (type, `source_path`).
fn gemini_current_install() -> Option<(String, Option<String>)> {
    let home = dirs::home_dir()?;
    let ext_dir = home.join(".gemini/extensions");
    let ext_path = ["Catenary", "catenary"]
        .iter()
        .map(|name| ext_dir.join(name))
        .find(|p| p.is_dir())?;

    let install_meta_path = ext_path.join(".gemini-extension-install.json");
    let meta = std::fs::read_to_string(&install_meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());

    let install_type = meta
        .as_ref()
        .and_then(|m| m.get("type").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown")
        .to_string();

    let source = meta
        .as_ref()
        .and_then(|m| m.get("source").and_then(serde_json::Value::as_str))
        .map(std::string::ToString::to_string);

    Some((install_type, source))
}

// ── Antigravity CLI install ────────────────────────────────────────

/// Embedded Antigravity plugin files.
const AGY_PLUGIN_JSON: &str = include_str!("../../plugins/catenary-antigravity/plugin.json");
const AGY_MCP_CONFIG: &str = include_str!("../../plugins/catenary-antigravity/mcp_config.json");
const AGY_HOOKS: &str = include_str!("../../plugins/catenary-antigravity/hooks.json");
/// The Antigravity rules file — the host's only compaction-proof teaching leg
/// (teaching-surface ticket 10). Rules files re-inject per conversation turn, so
/// this file carries the SSOT static tiers (the `fallback_body()` render, runtime
/// data structurally excluded) with `trigger: always_on` frontmatter pinning
/// unconditional per-turn loading. It forms a hybrid with teach-03's
/// `PreInvocation` injection: the rules file carries the static tiers per turn
/// (compaction-proof), the persisted `userMessage` carries the live surface once
/// (and dies at compaction). Freshness is pinned by
/// `teaching::tests::shipped_antigravity_rules_are_fresh`.
const AGY_RULES: &str = include_str!("../../plugins/catenary-antigravity/rules/catenary.md");

/// Antigravity plugin file set for installation.
const AGY_FILES: &[(&str, &str)] = &[
    ("plugin.json", AGY_PLUGIN_JSON),
    ("mcp_config.json", AGY_MCP_CONFIG),
    ("hooks.json", AGY_HOOKS),
    ("rules/catenary.md", AGY_RULES),
];

/// Run `catenary install antigravity`.
///
/// # Errors
///
/// Returns an error if file operations fail.
pub fn run_install_antigravity(
    out: &mut Output,
    source: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let _ = out.writeln(format_args!("Antigravity CLI:"));

    let parsed_source = source.map(parse_source);

    let home = dirs::home_dir().context("cannot determine home directory")?;
    let target_dir = home.join(".gemini/config/plugins/catenary");

    match parsed_source {
        Some(InstallSource::Local(path)) => {
            install_antigravity_local(out, &path, &target_dir, dry_run)?;
        }
        Some(InstallSource::Remote(_)) | None => {
            install_antigravity_bundled(out, &target_dir, dry_run)?;
        }
    }

    Ok(())
}

/// Install Antigravity plugin via symlink to local path.
fn install_antigravity_local(
    out: &mut Output,
    source: &Path,
    target: &Path,
    dry_run: bool,
) -> Result<()> {
    // Source should point to the antigravity plugin directory
    let plugin_dir = source.join("plugins/catenary-antigravity");
    let source_dir = if plugin_dir.is_dir() {
        plugin_dir
    } else {
        source.to_path_buf()
    };

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} symlink {} → {}",
            out.colors.dim("(dry-run)"),
            target.display(),
            source_dir.display(),
        ));
        return Ok(());
    }

    // Remove existing directory or symlink
    if target.is_symlink() || target.is_dir() {
        if target.is_symlink() {
            std::fs::remove_file(target)
                .with_context(|| format!("remove existing symlink at {}", target.display()))?;
        } else {
            std::fs::remove_dir_all(target)
                .with_context(|| format!("remove existing directory at {}", target.display()))?;
        }
    }

    // Ensure parent directory exists
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent directory {}", parent.display()))?;
    }

    // Create symlink
    #[cfg(unix)]
    std::os::unix::fs::symlink(&source_dir, target)
        .with_context(|| format!("symlink {} → {}", target.display(), source_dir.display()))?;

    #[cfg(not(unix))]
    {
        anyhow::bail!("symlink install not supported on this platform");
    }

    let _ = out.writeln(format_args!(
        "  {} symlinked → {}",
        out.colors.green("✓"),
        source_dir.display(),
    ));

    Ok(())
}

/// Install Antigravity plugin by copying embedded files.
fn install_antigravity_bundled(out: &mut Output, target: &Path, dry_run: bool) -> Result<()> {
    // Check if target is a symlink — that means dev install, don't overwrite
    if target.is_symlink() {
        let link_target = std::fs::read_link(target).unwrap_or_default();
        let _ = out.writeln(format_args!(
            "  {} symlinked → {} (use explicit source to switch)",
            out.colors.green("✓"),
            link_target.display(),
        ));
        return Ok(());
    }

    // Check staleness
    let mut stale_count = 0;
    let mut up_to_date = true;

    for (rel_path, expected_content) in AGY_FILES {
        let file_path = target.join(rel_path);
        match std::fs::read_to_string(&file_path) {
            Ok(current) if current == *expected_content => {}
            _ => {
                stale_count += 1;
                up_to_date = false;
            }
        }
    }

    if up_to_date {
        let _ = out.writeln(format_args!(
            "  {} plugin files up to date",
            out.colors.green("✓"),
        ));
        return Ok(());
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} write {stale_count} file(s) to {}",
            out.colors.dim("(dry-run)"),
            target.display(),
        ));
        return Ok(());
    }

    // Write files
    for (rel_path, content) in AGY_FILES {
        let file_path = target.join(rel_path);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
        std::fs::write(&file_path, content)
            .with_context(|| format!("write {}", file_path.display()))?;
    }

    let _ = out.writeln(format_args!(
        "  {} wrote {stale_count} file(s) to {}",
        out.colors.green("✓"),
        target.display(),
    ));

    Ok(())
}

// ── OpenCode install ───────────────────────────────────────────────

/// Embedded OpenCode plugin file.
const OC_PLUGIN_JS: &str = include_str!("../../plugins/catenary-opencode/catenary.js");

/// Resolved install targets for OpenCode. Integration is plugin-only: the single
/// Catenary-owned plugin file is all that lands, and the user-owned
/// `opencode.json` is never touched (the MCP heartbeat and the runtime teaching
/// ride the plugin's `config` hook, not a JSON merge).
struct OpenCodeTargets {
    /// Auto-discovered plugin file (`plugin/catenary.js`).
    plugin: PathBuf,
}

/// Resolve OpenCode install targets for the global (`~/.config/opencode/`) or
/// workspace (`<cwd>/.opencode/`) location.
fn opencode_targets(workspace: bool) -> Result<OpenCodeTargets> {
    if workspace {
        let root = std::env::current_dir().context("cannot determine current directory")?;
        Ok(OpenCodeTargets {
            plugin: root.join(".opencode/plugin/catenary.js"),
        })
    } else {
        let home = dirs::home_dir().context("cannot determine home directory")?;
        Ok(OpenCodeTargets {
            plugin: home.join(".config/opencode/plugin/catenary.js"),
        })
    }
}

/// Run `catenary install opencode [--workspace]`.
///
/// Integration is plugin-only: writes (or symlinks) the auto-discovered plugin
/// file — the only artifact. The user-owned `opencode.json` is never created or
/// modified; the MCP heartbeat and the runtime teaching ride the plugin's
/// `config` hook. No user-authored `AGENTS.md` is touched either.
///
/// # Errors
///
/// Returns an error if file operations fail.
pub fn run_install_opencode(
    out: &mut Output,
    source: Option<&str>,
    workspace: bool,
    dry_run: bool,
) -> Result<()> {
    let _ = out.writeln(format_args!("OpenCode:"));

    let targets = opencode_targets(workspace)?;
    install_opencode(out, source, &targets, dry_run)
}

/// Install the Catenary-owned OpenCode plugin file into `targets`. Never touches
/// `opencode.json`. Split from [`run_install_opencode`] so tests can drive it
/// against tempdir targets.
fn install_opencode(
    out: &mut Output,
    source: Option<&str>,
    targets: &OpenCodeTargets,
    dry_run: bool,
) -> Result<()> {
    match source.map(parse_source) {
        Some(InstallSource::Local(path)) => install_opencode_local(out, &path, targets, dry_run),
        Some(InstallSource::Remote(_)) | None => install_opencode_bundled(out, targets, dry_run),
    }
}

/// Install the OpenCode plugin by symlinking to a local path (dev mode).
fn install_opencode_local(
    out: &mut Output,
    source: &Path,
    targets: &OpenCodeTargets,
    dry_run: bool,
) -> Result<()> {
    // Resolve the source plugin file: a repo root, the plugin dir, or the file.
    let in_repo = source.join("plugins/catenary-opencode/catenary.js");
    let in_dir = source.join("catenary.js");
    let source_file = if in_repo.is_file() {
        in_repo
    } else if in_dir.is_file() {
        in_dir
    } else {
        source.to_path_buf()
    };

    let target = &targets.plugin;

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} symlink {} → {}",
            out.colors.dim("(dry-run)"),
            target.display(),
            source_file.display(),
        ));
        return Ok(());
    }

    // Remove any existing plugin file or symlink.
    if target.is_symlink() || target.exists() {
        std::fs::remove_file(target)
            .with_context(|| format!("remove existing plugin at {}", target.display()))?;
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create plugin directory {}", parent.display()))?;
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(&source_file, target)
        .with_context(|| format!("symlink {} → {}", target.display(), source_file.display()))?;

    #[cfg(not(unix))]
    {
        anyhow::bail!("symlink install not supported on this platform");
    }

    let _ = out.writeln(format_args!(
        "  {} plugin symlinked → {}",
        out.colors.green("✓"),
        source_file.display(),
    ));

    Ok(())
}

/// Install the OpenCode plugin by writing the embedded file (bundled mode).
fn install_opencode_bundled(
    out: &mut Output,
    targets: &OpenCodeTargets,
    dry_run: bool,
) -> Result<()> {
    let target = &targets.plugin;

    // A symlink means a dev install — don't overwrite it.
    if target.is_symlink() {
        let link_target = std::fs::read_link(target).unwrap_or_default();
        let _ = out.writeln(format_args!(
            "  {} plugin symlinked → {} (use explicit source to switch)",
            out.colors.green("✓"),
            link_target.display(),
        ));
        return Ok(());
    }

    let up_to_date = std::fs::read_to_string(target).is_ok_and(|c| c == OC_PLUGIN_JS);
    if up_to_date {
        let _ = out.writeln(format_args!(
            "  {} plugin up to date",
            out.colors.green("✓"),
        ));
        return Ok(());
    }

    if dry_run {
        let _ = out.writeln(format_args!(
            "  {} write plugin to {}",
            out.colors.dim("(dry-run)"),
            target.display(),
        ));
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create plugin directory {}", parent.display()))?;
    }
    std::fs::write(target, OC_PLUGIN_JS).with_context(|| format!("write {}", target.display()))?;

    let _ = out.writeln(format_args!(
        "  {} wrote plugin to {}",
        out.colors.green("✓"),
        target.display(),
    ));

    Ok(())
}

// ── Post-update refresh ───────────────────────────────────────────

/// Refresh all hosts that have an existing Catenary installation.
///
/// Called by `catenary update` after binary replacement. Runs
/// `install <host>` with no source argument for each host that
/// already has Catenary installed, preserving the existing
/// local/release registration.
///
/// # Errors
///
/// Returns an error if any install step fails.
pub fn refresh_installed_hosts(out: &mut Output) -> Result<()> {
    let claude = binary_exists("claude") && claude_plugin_is_installed();
    let gemini = binary_exists("gemini") && gemini_current_install().is_some();
    let antigravity = dirs::home_dir()
        .map(|h| h.join(".gemini/config/plugins/catenary"))
        .is_some_and(|p| p.is_dir() || p.is_symlink());
    let opencode = dirs::home_dir()
        .map(|h| h.join(".config/opencode/plugin/catenary.js"))
        .is_some_and(|p| p.is_file() || p.is_symlink());

    if claude {
        run_install_claude(out, None, false)?;
    }
    if gemini {
        run_install_gemini(out, None, false)?;
    }
    if antigravity {
        run_install_antigravity(out, None, false)?;
    }
    if opencode {
        run_install_opencode(out, None, false, false)?;
    }

    if !claude && !gemini && !antigravity && !opencode {
        let _ = out.writeln(format_args!(
            "  {} no hosts have Catenary installed",
            out.colors.dim("—"),
        ));
    }

    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Source parsing ──────────────────────────────────────────────

    #[test]
    fn parse_source_absolute_path() {
        let src = parse_source("/home/user/Projects/Catenary");
        let InstallSource::Local(p) = src else {
            unreachable!("expected Local source");
        };
        assert_eq!(p, PathBuf::from("/home/user/Projects/Catenary"));
    }

    #[test]
    fn parse_source_relative_path() {
        let src = parse_source("./Catenary");
        let InstallSource::Local(p) = src else {
            unreachable!("expected Local source");
        };
        assert_eq!(p, PathBuf::from("./Catenary"));
    }

    #[test]
    fn parse_source_repo_identifier() {
        let src = parse_source("TwoWells/Catenary");
        let InstallSource::Remote(r) = src else {
            unreachable!("expected Remote source");
        };
        assert_eq!(r, "TwoWells/Catenary");
    }

    // ── Antigravity bundled files ──────────────────────────────────

    #[test]
    fn embedded_antigravity_files_not_empty() {
        for (name, content) in AGY_FILES {
            assert!(
                !content.is_empty(),
                "embedded file {name} should not be empty",
            );
        }
    }

    #[test]
    fn antigravity_hooks_register_pre_invocation_teaching() {
        // Teaching-surface ticket 03: the shipped Antigravity hooks.json wires the
        // `PreInvocation` first-sighting teaching injection. Per the Antigravity
        // hook contract, `PreInvocation` is a *flat* array of handler objects (no
        // `matcher`/`hooks` wrapper), unlike `PreToolUse`.
        let hooks: serde_json::Value =
            serde_json::from_str(AGY_HOOKS).expect("AGY_HOOKS is valid JSON");
        let pre = hooks["catenary-editing"]["PreInvocation"]
            .as_array()
            .expect("PreInvocation flat array present");
        assert!(
            pre.iter().any(|h| h["command"].as_str()
                == Some("catenary hook pre-invocation --format=antigravity")),
            "PreInvocation must invoke the antigravity pre-invocation hook: {hooks}",
        );
    }

    #[test]
    fn antigravity_bundled_creates_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let target = dir.path().join("catenary");

        let mut out = Output::buffer(80);
        install_antigravity_bundled(&mut out, &target, false).expect("install should succeed");

        for (rel_path, expected) in AGY_FILES {
            let file = target.join(rel_path);
            let content = std::fs::read_to_string(&file).expect("installed file should exist");
            assert_eq!(content, *expected, "{rel_path} content should match");
        }

        let output = out.into_string();
        assert!(output.contains("wrote"), "output: {output}");
    }

    #[test]
    fn antigravity_bundled_idempotent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let target = dir.path().join("catenary");

        let mut out = Output::buffer(80);
        install_antigravity_bundled(&mut out, &target, false).expect("first install");

        let mut out2 = Output::buffer(80);
        install_antigravity_bundled(&mut out2, &target, false).expect("second install");
        let output = out2.into_string();
        assert!(output.contains("up to date"), "output: {output}");
    }

    #[test]
    fn antigravity_bundled_dry_run_no_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let target = dir.path().join("catenary");

        let mut out = Output::buffer(80);
        install_antigravity_bundled(&mut out, &target, true).expect("dry run");

        assert!(!target.exists(), "dry run should not create directory");
        let output = out.into_string();
        assert!(output.contains("(dry-run)"), "output: {output}");
    }

    #[test]
    fn antigravity_bundled_staleness_rewrites_rules() {
        // Teaching-surface ticket 10: a stale `rules/catenary.md` (the
        // compaction-proof teaching leg) must be rewritten by a re-run of
        // `catenary install antigravity` — the same staleness machinery the other
        // AGY files ride. A stale body under the correct frontmatter still trips
        // the byte-for-byte staleness check and is overwritten with the shipped
        // content.
        let dir = tempfile::tempdir().expect("create tempdir");
        let target = dir.path().join("catenary");
        let rules = target.join("rules/catenary.md");
        std::fs::create_dir_all(rules.parent().expect("rules parent")).expect("create rules dir");
        std::fs::write(&rules, "---\ntrigger: always_on\n---\n\nstale body\n")
            .expect("write stale rules");

        let mut out = Output::buffer(80);
        install_antigravity_bundled(&mut out, &target, false).expect("rewrite stale");

        assert_eq!(
            std::fs::read_to_string(&rules).expect("rules should exist"),
            AGY_RULES,
            "stale rules/catenary.md should be rewritten to the shipped content",
        );
        assert!(out.into_string().contains("wrote"));
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_local_creates_symlink() {
        let source_dir = tempfile::tempdir().expect("create source dir");
        let target_dir = tempfile::tempdir().expect("create target dir");
        let target = target_dir.path().join("catenary");

        let mut out = Output::buffer(80);
        install_antigravity_local(&mut out, source_dir.path(), &target, false)
            .expect("symlink install should succeed");

        assert!(target.is_symlink(), "target should be a symlink");
        let output = out.into_string();
        assert!(output.contains("symlinked"), "output: {output}");
    }

    #[cfg(unix)]
    #[test]
    fn antigravity_local_replaces_directory_with_symlink() {
        let source_dir = tempfile::tempdir().expect("create source dir");
        let target_dir = tempfile::tempdir().expect("create target dir");
        let target = target_dir.path().join("catenary");

        // Create existing directory
        std::fs::create_dir_all(&target).expect("create target dir");
        std::fs::write(target.join("old_file.txt"), "old").expect("write old file");

        let mut out = Output::buffer(80);
        install_antigravity_local(&mut out, source_dir.path(), &target, false)
            .expect("symlink install should replace dir");

        assert!(target.is_symlink(), "target should be a symlink");
    }

    // ── OpenCode bundled files ─────────────────────────────────────

    fn oc_targets(base: &Path) -> OpenCodeTargets {
        OpenCodeTargets {
            plugin: base.join("plugin/catenary.js"),
        }
    }

    #[test]
    fn embedded_opencode_files_not_empty() {
        assert!(!OC_PLUGIN_JS.is_empty(), "plugin js should not be empty");
    }

    #[test]
    fn opencode_bundled_creates_plugin() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());

        let mut out = Output::buffer(80);
        install_opencode_bundled(&mut out, &targets, false).expect("install should succeed");

        let content = std::fs::read_to_string(&targets.plugin).expect("plugin should exist");
        assert_eq!(content, OC_PLUGIN_JS);
        assert!(out.into_string().contains("wrote plugin"));
    }

    #[test]
    fn opencode_bundled_idempotent() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());

        let mut out = Output::buffer(80);
        install_opencode_bundled(&mut out, &targets, false).expect("first install");

        let mut out2 = Output::buffer(80);
        install_opencode_bundled(&mut out2, &targets, false).expect("second install");
        assert!(out2.into_string().contains("up to date"));
    }

    #[test]
    fn opencode_bundled_staleness_rewrites() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());
        std::fs::create_dir_all(targets.plugin.parent().expect("parent"))
            .expect("create plugin dir");
        std::fs::write(&targets.plugin, "// stale").expect("write stale plugin");

        let mut out = Output::buffer(80);
        install_opencode_bundled(&mut out, &targets, false).expect("rewrite stale");

        let content = std::fs::read_to_string(&targets.plugin).expect("plugin should exist");
        assert_eq!(content, OC_PLUGIN_JS);
        assert!(out.into_string().contains("wrote plugin"));
    }

    #[test]
    fn opencode_bundled_dry_run_no_files() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());

        let mut out = Output::buffer(80);
        install_opencode_bundled(&mut out, &targets, true).expect("dry run");

        assert!(!targets.plugin.exists(), "dry run should not write plugin");
        assert!(out.into_string().contains("(dry-run)"));
    }

    #[cfg(unix)]
    #[test]
    fn opencode_local_creates_symlink() {
        let source_dir = tempfile::tempdir().expect("create source dir");
        let source_file = source_dir.path().join("catenary.js");
        std::fs::write(&source_file, OC_PLUGIN_JS).expect("write source plugin");

        let target_dir = tempfile::tempdir().expect("create target dir");
        let targets = oc_targets(target_dir.path());

        let mut out = Output::buffer(80);
        install_opencode_local(&mut out, source_dir.path(), &targets, false)
            .expect("symlink install should succeed");

        assert!(targets.plugin.is_symlink(), "plugin should be a symlink");
        assert!(out.into_string().contains("symlinked"));
    }

    // ── OpenCode is plugin-only (never touches opencode.json) ───────

    /// Integration is plugin-only: a full install writes only the two
    /// Catenary-owned files and never creates or modifies the user-owned
    /// `opencode.json`. A pre-existing config with a sentinel body must survive
    /// byte-for-byte.
    #[test]
    fn install_opencode_never_touches_config() {
        // A user config that would have tripped the old merge — a JSONC comment
        // header (bug 61) plus an already-present absolute rules entry
        // (finding 2). Both must now be left completely untouched.
        const SENTINEL: &str = "// chezmoi:managed — do not edit\n{\n  \"instructions\": [\"/home/user/.config/opencode/catenary.md\"]\n}\n";

        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());
        let config = dir.path().join("opencode.json");
        std::fs::write(&config, SENTINEL).expect("write sentinel config");

        let mut out = Output::buffer(80);
        install_opencode(&mut out, None, &targets, false).expect("install should succeed");

        // The one Catenary-owned file is written.
        assert_eq!(
            std::fs::read_to_string(&targets.plugin).expect("plugin should exist"),
            OC_PLUGIN_JS,
        );

        // The user config is byte-identical — never parsed, never rewritten.
        assert_eq!(
            std::fs::read_to_string(&config).expect("config still exists"),
            SENTINEL,
            "install must not modify opencode.json",
        );
    }

    /// Absent an `opencode.json`, a full install must not create one.
    #[test]
    fn install_opencode_creates_no_config() {
        let dir = tempfile::tempdir().expect("create tempdir");
        let targets = oc_targets(dir.path());
        let config = dir.path().join("opencode.json");

        let mut out = Output::buffer(80);
        install_opencode(&mut out, None, &targets, false).expect("install should succeed");

        assert!(
            !config.exists(),
            "install must not create opencode.json when none exists",
        );
    }

    // ── List subcommand ────────────────────────────────────────────

    #[test]
    fn install_list_output() {
        let mut out = Output::buffer(80);
        run_install_list(&mut out).expect("list should succeed");
        let output = out.into_string();
        assert!(output.contains("Detected hosts:"), "output: {output}");
    }

    // ── Claude marketplace refresh ──────────────────────────────────

    /// Registration-current ≠ content-current: an already-registered marketplace
    /// (`needs_update = false`) must STILL refresh its cached content, else a
    /// reinstall re-serves stale hooks and the daemon keeps flagging them.
    #[test]
    fn claude_marketplace_refreshes_even_when_already_registered() {
        let mut out = Output::buffer(80);
        let proceed = claude_ensure_marketplace(&mut out, "/some/dir", false, true)
            .expect("dry run should succeed");
        assert!(proceed, "should proceed to plugin install");
        let output = out.into_string();
        assert!(
            output.contains(&format!("marketplace update {CLAUDE_MARKETPLACE}")),
            "must refresh content even when already registered. output: {output}"
        );
        assert!(
            !output.contains("marketplace add"),
            "must not re-add an already-registered marketplace. output: {output}"
        );
    }

    /// A new/changed source both registers (`add`) and refreshes (`update`).
    #[test]
    fn claude_marketplace_adds_then_refreshes_new_source() {
        let mut out = Output::buffer(80);
        claude_ensure_marketplace(&mut out, "/new/dir", true, true).expect("dry run");
        let output = out.into_string();
        assert!(
            output.contains("marketplace add /new/dir"),
            "new source must be registered. output: {output}"
        );
        assert!(
            output.contains(&format!("marketplace update {CLAUDE_MARKETPLACE}")),
            "new source must also be refreshed. output: {output}"
        );
    }

    // ── Per-host static instruction surfaces ────────────────────────

    #[test]
    fn install_writes_pointer_per_host() {
        // No host ships a primer *pointer* anymore. Claude Code and OpenCode
        // ship no static instruction surface at all: teaching-surface 11
        // dropped the `catenary:primer` skill as pure duplication (SessionStart
        // re-stamps the live payload, including on `compact`), and
        // teaching-surface 08 made OpenCode runtime-only (the plugin's `config`
        // hook regenerates its instructions).
        //
        // Antigravity keeps a static artifact as a compaction-proof leg
        // (teaching-surface 10): a bootstrap/fallback that inlines the SSOT
        // teaching (runtime data excluded) rather than pointing at the primer —
        // the live surface rides the runtime channel (the `PreInvocation` sliver).
        // Its freshness is pinned in `cli::teaching`
        // (`shipped_antigravity_rules_are_fresh`). Gemini retired its static
        // context file in teaching-surface 14 — its teaching is hook-only
        // (`SessionStart` plus the `PreCompress`/`BeforeAgent` discontinuity
        // re-injection), so no static Gemini surface remains to check.
        //
        // The Antigravity fallback inlines the teaching — no primer pointer, and
        // no runtime data (the allow surface / build tool only ride the runtime
        // channel).
        assert!(
            !AGY_RULES.contains("catenary primer"),
            "the antigravity fallback should inline the teaching, not point at the primer",
        );
        assert!(
            AGY_RULES.contains("The edit→diagnostics loop"),
            "the antigravity fallback should inline the SSOT invariants",
        );
    }
}
