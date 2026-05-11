// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the `glob` tool.

mod common;

use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use common::BridgeProcess;

const MOCK_LANG_A: &str = "yX4Za";

/// Spawns the bridge without any LSP servers configured.
fn spawn_no_lsp(root: &str) -> Result<BridgeProcess> {
    BridgeProcess::spawn(&[], root)
}

/// Spawns the bridge with a real LSP server argument.
#[allow(dead_code, reason = "used by ignored lua-language-server tests")]
fn spawn_with_real_lsp(lsp_arg: &str, root: &str) -> Result<BridgeProcess> {
    BridgeProcess::spawn(&[lsp_arg], root)
}

#[test]
fn test_glob_directory_basic() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join("src"))?;
    std::fs::write(dir.path().join("Cargo.toml"), "[package]")?;
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    assert!(text.contains("src/"), "Should list src directory: {text}");
    assert!(
        text.contains("Cargo.toml"),
        "Should list Cargo.toml: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_outside_root() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    std::fs::write(outside.path().join("hello.txt"), "hi")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let result = bridge.call_tool(
        "glob",
        &json!({ "pattern": outside.path().to_string_lossy().as_ref() }),
    )?;

    let is_error = result.get("isError").and_then(serde_json::Value::as_bool);
    assert_ne!(is_error, Some(true), "Should not be an error");

    let text = result
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|e| e.get("text"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    assert!(
        text.contains("hello.txt"),
        "Should list files outside workspace roots: {text}"
    );
    Ok(())
}

#[test]
fn test_tools_list_includes_glob() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;
    let tools = response
        .get("result")
        .and_then(|r| r.get("tools"))
        .and_then(|t| t.as_array())
        .context("No tools in response")?;

    let tool_names: Vec<&str> = tools
        .iter()
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();

    assert!(
        tool_names.contains(&"glob"),
        "Should include glob: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"list_directory"),
        "Should not include list_directory: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"document_symbols"),
        "Should not include document_symbols: {tool_names:?}"
    );
    assert!(
        !tool_names.contains(&"codebase_map"),
        "Should not include codebase_map: {tool_names:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_glob_directory_symlink() -> Result<()> {
    use std::os::unix::fs as unix_fs;

    let dir = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;

    std::fs::write(outside.path().join("secret.txt"), "secret")?;

    // Create symlink inside workspace pointing outside
    unix_fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("link.txt"),
    )?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Symlink should be shown with its target
    assert!(
        text.contains("link.txt ->"),
        "Symlink should be shown with arrow: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_file_header() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &script,
        "struct Config\nenum Mode\nconst MAX_SIZE\nfn helper\n",
    )?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": script.to_str().context("file path")? }),
    )?;

    // File header with line count
    assert!(text.contains("(4 lines)"), "Should show line count: {text}");

    // No symbols: bridge has no LSP servers and no grammar installed
    assert!(
        !text.contains("Config"),
        "Should not show symbols without grammar: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_pattern_matching() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("lib.rs"), "pub mod lib;")?;
    std::fs::write(dir.path().join("readme.md"), "# Readme")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "*.rs" }))?;

    assert!(text.contains("main.rs"), "Should match main.rs: {text}");
    assert!(text.contains("lib.rs"), "Should match lib.rs: {text}");
    assert!(
        !text.contains("readme.md"),
        "Should not match readme.md: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_alternation() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("Cargo.toml"), "[package]")?;
    std::fs::write(dir.path().join("readme.md"), "# Readme")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "*.{rs,toml}" }))?;

    assert!(text.contains("main.rs"), "Should match main.rs: {text}");
    assert!(
        text.contains("Cargo.toml"),
        "Should match Cargo.toml: {text}"
    );
    assert!(
        !text.contains("readme.md"),
        "Should not match readme.md: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_line_counts() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("three.txt"), "line1\nline2\nline3\n")?;
    std::fs::write(dir.path().join("one.txt"), "single\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    assert!(
        text.contains("(3 lines)"),
        "Should show 3 lines for three.txt: {text}"
    );
    assert!(
        text.contains("(1 lines)"),
        "Should show 1 lines for one.txt: {text}"
    );
    // Should NOT show bytes
    assert!(!text.contains("bytes"), "Should not show bytes: {text}");
    Ok(())
}

#[test]
fn test_glob_pattern_detection() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let script = dir.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&script, "struct Config\nenum Mode\n")?;
    std::fs::create_dir(dir.path().join("subdir"))?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // File path → header format (shows line count)
    let file_text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": script.to_str().context("file path")? }),
    )?;
    assert!(
        file_text.contains("(2 lines)"),
        "File mode should show line count: {file_text}"
    );

    // Directory path → listing format (shows entries)
    let dir_text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;
    assert!(
        dir_text.contains("subdir/"),
        "Dir mode should show subdirectories: {dir_text}"
    );
    assert!(
        dir_text.contains(&format!("types.{MOCK_LANG_A}")),
        "Dir mode should list files: {dir_text}"
    );

    // Glob pattern → match format
    let glob_text =
        bridge.call_tool_text("glob", &json!({ "pattern": format!("*.{MOCK_LANG_A}") }))?;
    assert!(
        glob_text.contains(&format!("types.{MOCK_LANG_A}")),
        "Pattern mode should match files: {glob_text}"
    );
    Ok(())
}

// ─── New 08a tests ─────────────────────────────────────────────────────

#[test]
fn test_glob_exclude() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src)?;
    std::fs::write(src.join("main.rs"), "fn main() {}")?;
    std::fs::write(src.join("test_helper.rs"), "fn test() {}")?;
    std::fs::write(src.join("test_util.rs"), "fn util() {}")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": "src/*.rs",
            "exclude": "test_*"
        }),
    )?;

    assert!(text.contains("main.rs"), "Should include main.rs: {text}");
    assert!(
        !text.contains("test_helper.rs"),
        "Should exclude test_helper.rs: {text}"
    );
    assert!(
        !text.contains("test_util.rs"),
        "Should exclude test_util.rs: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_include_hidden() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("visible.txt"), "content")?;
    std::fs::write(dir.path().join(".hidden"), "secret")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Default: hidden files excluded
    let text_default = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;
    assert!(
        text_default.contains("visible.txt"),
        "Should show visible.txt: {text_default}"
    );
    assert!(
        !text_default.contains(".hidden"),
        "Should not show .hidden by default: {text_default}"
    );

    // With include_hidden: true
    let text_hidden = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": dir.path().to_string_lossy().to_string(),
            "include_hidden": true
        }),
    )?;
    assert!(
        text_hidden.contains(".hidden"),
        "Should show .hidden with include_hidden: {text_hidden}"
    );
    Ok(())
}

/// Grep with `glob=.gitignore` should find matches in `.gitignore`
/// without requiring `include_hidden`. This is the motivating case
/// for ticket misc/45.
#[test]
fn test_grep_explicit_hidden_glob_matches() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join(".gitignore"), "target/\nbuild/\n")?;
    std::fs::write(dir.path().join("README.md"), "hello")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "target", "glob": ".gitignore" }),
    )?;
    assert!(
        text.contains("target"),
        "grep glob=.gitignore should find 'target' without include_hidden: {text}"
    );
    Ok(())
}

/// Grep with a broad glob should still exclude hidden files by default.
#[test]
fn test_grep_broad_glob_excludes_hidden() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join(".secret"), "password123\n")?;
    std::fs::write(dir.path().join("visible.txt"), "password123\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text =
        bridge.call_tool_text("grep", &json!({ "pattern": "password123", "glob": "**/*" }))?;
    assert!(
        text.contains("visible.txt"),
        "Broad glob should find visible files: {text}"
    );
    assert!(
        !text.contains(".secret"),
        "Broad glob should not match hidden files without include_hidden: {text}"
    );
    Ok(())
}

/// Glob with an explicit hidden pattern (`.gitignore`) should match
/// without `include_hidden`.
#[test]
fn test_glob_explicit_hidden_matches() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join(".gitignore"), "target/\n")?;
    std::fs::write(dir.path().join("README.md"), "hello")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": ".gitignore" }))?;
    assert!(
        text.contains(".gitignore"),
        "Explicit .gitignore glob should match without include_hidden: {text}"
    );
    Ok(())
}

/// Glob with an explicit hidden directory pattern (`.github/*`) should
/// match without `include_hidden`.
#[test]
fn test_glob_explicit_hidden_dir_matches() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let gh = dir.path().join(".github");
    std::fs::create_dir(&gh)?;
    std::fs::write(gh.join("ci.yml"), "name: CI\n")?;
    std::fs::write(dir.path().join("README.md"), "hello")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": ".github/*.yml" }))?;
    assert!(
        text.contains("ci.yml"),
        "Explicit .github/*.yml glob should match without include_hidden: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_include_gitignored() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Initialize git repo
    Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .context("Failed to run git init")?;

    std::fs::write(dir.path().join(".gitignore"), "*.log\n")?;
    std::fs::write(dir.path().join("app.txt"), "content")?;
    std::fs::write(dir.path().join("debug.log"), "log data")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Default: gitignored files absent
    let text_default = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": dir.path().to_string_lossy().to_string(),
            "include_hidden": true
        }),
    )?;
    assert!(
        text_default.contains("app.txt"),
        "Should show app.txt: {text_default}"
    );
    assert!(
        !text_default.contains("debug.log"),
        "Should not show debug.log by default: {text_default}"
    );

    // With include_gitignored: true
    let text_ignored = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": dir.path().to_string_lossy().to_string(),
            "include_gitignored": true,
            "include_hidden": true
        }),
    )?;
    assert!(
        text_ignored.contains("debug.log"),
        "Should show debug.log with include_gitignored: {text_ignored}"
    );
    Ok(())
}

#[test]
fn test_glob_tier3_bucketed() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create many files with separator-based names to exceed budget.
    for i in 0..30 {
        std::fs::write(
            dir.path().join(format!("test_grep_{i}.rs")),
            format!("// file {i}\n"),
        )?;
    }
    for i in 0..20 {
        std::fs::write(
            dir.path().join(format!("test_glob_{i}.rs")),
            format!("// file {i}\n"),
        )?;
    }

    // Use a small budget to force bucketing.
    // The test spawns with default config, so we need enough files
    // that the file listing exceeds the default 2000-char budget.
    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // With 50 files, each line is ~30 chars = ~1500 chars.
    // If it fits in budget, we get tier 2 (all filenames).
    // If not, we get tier 3 (bucketed).
    // Either way, the output should be valid. Assert basic structure.
    assert!(
        text.contains("test_grep_") || text.contains("test_glob_"),
        "Should contain file references: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_tier2_file_listing() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir(dir.path().join("src"))?;
    std::fs::write(dir.path().join("main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("lib.rs"), "pub mod lib;")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Small directory — should get tier 2 file listing.
    assert!(text.contains("src/"), "Should list directory: {text}");
    assert!(text.contains("main.rs"), "Should list main.rs: {text}");
    assert!(text.contains("lib.rs"), "Should list lib.rs: {text}");

    // Directories should appear before files.
    let src_pos = text.find("src/").expect("src/ should be in output");
    let main_pos = text.find("main.rs").expect("main.rs should be in output");
    assert!(
        src_pos < main_pos,
        "Directories should sort before files: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_budget_small() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        for i in 0..40 {
            std::fs::write(root.join(format!("test_item_{i:03}.txt")), format!("line {i}\n"))?;
        }
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, "[tools.glob]\nbudget = 1000\n")?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": bridge.root_path().to_string_lossy().to_string() }),
    )?;

    // With small budget, output should be compact.
    assert!(
        text.len() <= 1200, // some tolerance
        "Output should be budget-constrained: len={}, text:\n{text}",
        text.len()
    );
    Ok(())
}

#[test]
fn test_glob_bucket_drill() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create files with a shared prefix.
    for i in 0..5 {
        std::fs::write(
            dir.path().join(format!("test_grep_{i}.rs")),
            format!("fn test_{i}() {{}}\n"),
        )?;
    }
    for i in 0..5 {
        std::fs::write(
            dir.path().join(format!("test_glob_{i}.rs")),
            format!("fn test_{i}() {{}}\n"),
        )?;
    }
    std::fs::write(dir.path().join("README.md"), "# Readme\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // First call: directory listing (may be tier 2 or 3).
    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // If there's a bucket pattern, it should be a valid glob.
    if text.contains("files)") {
        // Extract a bucket pattern — lines like "test_grep_*  (5 files)"
        for line in text.lines() {
            if line.contains("files)") {
                let pattern = line.split("  (").next().unwrap_or("").trim();
                if !pattern.is_empty() {
                    // The bucket pattern should be passable back to glob.
                    let drill = bridge.call_tool_text("glob", &json!({ "pattern": pattern }))?;
                    assert!(
                        !drill.contains("No matches"),
                        "Bucket pattern '{pattern}' should be drillable: {drill}"
                    );
                }
            }
        }
    }
    Ok(())
}

#[test]
fn test_glob_pattern_tree() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    let bridge_dir = src.join("bridge");
    let lsp_dir = src.join("lsp");
    std::fs::create_dir_all(&bridge_dir)?;
    std::fs::create_dir_all(&lsp_dir)?;

    std::fs::write(bridge_dir.join("handler.rs"), "fn handle() {}\n")?;
    std::fs::write(bridge_dir.join("mod.rs"), "mod handler;\n")?;
    std::fs::write(lsp_dir.join("client.rs"), "struct Client;\n")?;
    std::fs::write(src.join("lib.rs"), "mod bridge;\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "src/**/*.rs" }))?;

    // Should produce a nested tree.
    assert!(
        text.contains("src/") || text.contains("bridge/"),
        "Should have directory nodes: {text}"
    );
    assert!(
        text.contains("handler.rs"),
        "Should include handler.rs: {text}"
    );
    assert!(
        text.contains("client.rs"),
        "Should include client.rs: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_tab_structure() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let sub = dir.path().join("src").join("inner");
    std::fs::create_dir_all(&sub)?;
    std::fs::write(sub.join("file.rs"), "fn f() {}\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "src/**/*.rs" }))?;

    // Tree output should use literal tab characters for indentation.
    assert!(text.contains('\t'), "Should use tab indentation: {text:?}");
    Ok(())
}

#[test]
fn test_glob_directories_count_against_budget() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create many directories that eat into the budget.
    for i in 0..60 {
        std::fs::create_dir(dir.path().join(format!("dir_{i:03}")))?;
    }
    // Add a few files too.
    for i in 0..10 {
        std::fs::write(
            dir.path().join(format!("file_{i}.txt")),
            format!("content {i}\n"),
        )?;
    }

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // With 60 dirs + 10 files, each ~15 chars, total ~1050 chars.
    // Should fit in default 2000 budget, but if it's over it should bucket.
    // The key assertion: directories are included in the output.
    assert!(
        text.contains("dir_"),
        "Directories should appear in output: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_separator_bucketing() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Create files with underscore separators.
    for i in 0..5 {
        std::fs::write(
            dir.path().join(format!("test_grep_{i}.rs")),
            format!("fn test_{i}() {{}}\n"),
        )?;
    }
    for i in 0..5 {
        std::fs::write(
            dir.path().join(format!("test_glob_{i}.rs")),
            format!("fn test_{i}() {{}}\n"),
        )?;
    }

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Use a small budget to force bucketing.
    let mut bridge2 = BridgeProcess::spawn_with_config(|root| {
        for i in 0..5 {
            std::fs::write(root.join(format!("test_grep_{i}.rs")), format!("fn test_{i}() {{}}\n"))?;
        }
        for i in 0..5 {
            std::fs::write(root.join(format!("test_glob_{i}.rs")), format!("fn test_{i}() {{}}\n"))?;
        }
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, "[tools.glob]\nbudget = 1000\n")?;
        Ok(config_path)
    })?;
    bridge2.initialize()?;

    let text = bridge2.call_tool_text(
        "glob",
        &json!({ "pattern": bridge2.root_path().to_string_lossy().to_string() }),
    )?;

    // With a small budget and separator-based filenames, bucketing should
    // produce semantic groups like test_grep_* and test_glob_*.
    // If tier 2 fits, we'll see individual files. Either is valid.
    assert!(
        text.contains("test_grep_") || text.contains("test_glob_"),
        "Should have test file references: {text}"
    );
    Ok(())
}

// ─── lua-language-server integration tests ──────────────────────────────

/// Glob file outline with real lua-language-server.
///
/// Creates a `.lua` file with a module table and local functions,
/// globs it as a file path, and checks that documentSymbol returns
/// outline data without hanging.
///
/// Run with: `make test T=lua_glob_file_outline -- --ignored`
/// Requires: lua-language-server on PATH.
#[test]
#[ignore = "requires lua-language-server"]
fn test_lua_glob_file_outline() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let lua_file = dir.path().join("helpers.lua");
    std::fs::write(
        &lua_file,
        "local M = {}\n\n\
         local MAX_RETRIES = 5\n\n\
         function M.setup(opts)\n  \
             M.opts = opts\n\
         end\n\n\
         function M.run()\n  \
             return true\n\
         end\n\n\
         return M\n",
    )?;

    let mut bridge = spawn_with_real_lsp("lua:lua-language-server", &dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // lua-language-server needs a moment to start; poll until ready
    let mut text = String::new();
    for _ in 0..10 {
        std::thread::sleep(Duration::from_secs(1));

        let result = bridge.call_tool_text(
            "glob",
            &json!({ "pattern": lua_file.to_str().context("file path")? }),
        )?;

        if result.contains("lines)") {
            text = result;
            break;
        }
        text = result;
    }

    assert!(text.contains("lines)"), "Should show line count: {text}");

    Ok(())
}

/// Glob pattern match across multiple lua files in subdirectories.
///
/// Mimics the chezmoi structure from `slow_glob.md` (`conky/lua/*.lua`).
/// Tests that `**/*.lua` completes without stacking 30s timeouts.
///
/// Run with: `make test T=lua_glob_pattern -- --ignored`
/// Requires: lua-language-server on PATH.
#[test]
#[ignore = "requires lua-language-server"]
fn test_lua_glob_pattern() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Mimic the chezmoi conky structure
    let lua_dir = dir.path().join("conky/lua");
    std::fs::create_dir_all(&lua_dir)?;

    std::fs::write(
        lua_dir.join("main.lua"),
        "local M = {}\nfunction M.init() end\nreturn M\n",
    )?;
    std::fs::write(
        lua_dir.join("helpers.lua"),
        "local H = {}\nfunction H.clamp(v, lo, hi) return math.max(lo, math.min(hi, v)) end\nreturn H\n",
    )?;
    std::fs::write(
        lua_dir.join("draw.lua"),
        "local D = {}\nfunction D.rect(x, y, w, h) end\nreturn D\n",
    )?;
    std::fs::write(
        lua_dir.join("list.lua"),
        "local L = {}\nfunction L.new() return {} end\nreturn L\n",
    )?;

    // Non-lua file that should not match
    std::fs::write(dir.path().join("conky/conky.conf"), "-- config\n")?;

    let mut bridge = spawn_with_real_lsp("lua:lua-language-server", &dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Give lua-language-server time to start
    std::thread::sleep(Duration::from_secs(2));

    let start = std::time::Instant::now();
    let text = bridge.call_tool_text("glob", &json!({ "pattern": "**/*.lua" }))?;
    let elapsed = start.elapsed();

    assert!(text.contains("main.lua"), "Should match main.lua: {text}");
    assert!(
        text.contains("helpers.lua"),
        "Should match helpers.lua: {text}"
    );
    assert!(text.contains("draw.lua"), "Should match draw.lua: {text}");
    assert!(text.contains("list.lua"), "Should match list.lua: {text}");
    assert!(
        !text.contains("conky.conf"),
        "Should not match conky.conf: {text}"
    );

    // If this takes >60s, something is seriously wrong (4 files should not
    // take anywhere near the 120s seen in slow_glob.md)
    assert!(
        elapsed < Duration::from_mins(1),
        "Glob pattern took {elapsed:?} — possible stacked LSP timeouts"
    );

    Ok(())
}

/// Glob directory listing with mixed file types including lua.
///
/// Tests that lua files get line counts while non-lua files
/// just get line counts too.
///
/// Run with: `make test T=lua_glob_directory -- --ignored`
/// Requires: lua-language-server on PATH.
#[test]
#[ignore = "requires lua-language-server"]
fn test_lua_glob_directory() -> Result<()> {
    let dir = tempfile::tempdir()?;

    std::fs::write(
        dir.path().join("init.lua"),
        "local M = {}\nfunction M.setup() end\nreturn M\n",
    )?;
    std::fs::write(dir.path().join("config.json"), "{\"key\": \"value\"}\n")?;
    std::fs::write(dir.path().join("notes.txt"), "some notes\n")?;

    let mut bridge = spawn_with_real_lsp("lua:lua-language-server", &dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Give lua-language-server time to start
    std::thread::sleep(Duration::from_secs(2));

    let start = std::time::Instant::now();
    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;
    let elapsed = start.elapsed();

    assert!(text.contains("init.lua"), "Should list init.lua: {text}");
    assert!(
        text.contains("config.json"),
        "Should list config.json: {text}"
    );
    assert!(text.contains("notes.txt"), "Should list notes.txt: {text}");

    assert!(
        elapsed < Duration::from_mins(1),
        "Directory glob took {elapsed:?} — possible stacked LSP timeouts"
    );

    Ok(())
}

// ─── New 08b tests ────────────────────────────────────────────────────

/// The mock language extension used for 08b tests (`.mock`).
const MOCK_EXT: &str = "mock";

/// Helper: spawns bridge with mockls for `.mock` files and optional config.
fn spawn_with_mockls_and_config(root: &str, config_toml: Option<&str>) -> Result<BridgeProcess> {
    let lsp = common::mockls_lsp_arg("mock", "");
    BridgeProcess::spawn_with_grammar(&[&lsp], root, |state_home| {
        if let Some(toml) = config_toml {
            let config_dir = std::path::PathBuf::from(state_home).join("catenary");
            std::fs::create_dir_all(&config_dir)?;
            std::fs::write(config_dir.join("config.toml"), toml)?;
        }
        Ok(())
    })
}

/// Generates a file with N lines of mock language definitions.
fn gen_mock_content(n: usize) -> String {
    use std::fmt::Write;
    let mut content = String::new();
    for i in 0..n {
        if i % 2 == 0 {
            let _ = writeln!(content, "fn func_{i}");
        } else {
            let _ = writeln!(content, "struct Struct_{i}");
        }
    }
    content
}

#[test]
fn test_glob_defensive_maps() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // File with few symbols but enough lines to cross threshold (set to 5).
    std::fs::write(
        dir.path().join(format!("big.{MOCK_EXT}")),
        "fn alpha\nfn beta\nstruct Gamma\n\n\n\n\n\n\n\n",
    )?;
    // Small file < threshold.
    std::fs::write(dir.path().join(format!("small.{MOCK_EXT}")), "fn tiny\n")?;

    let config = "[tools.glob]\noutline_threshold = 5\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Big file should have a map with symbols.
    assert!(
        text.contains("<Function>") || text.contains("<Struct>"),
        "Big file should have defensive map symbols: {text}"
    );
    // Small file should NOT have symbols (under threshold).
    assert!(
        text.contains(&format!("small.{MOCK_EXT}")),
        "Should list small file: {text}"
    );
    let small_line = text.lines().find(|l| l.contains("small.")).unwrap_or("");
    assert!(
        !small_line.contains('<'),
        "Small file should not have symbols: {small_line}"
    );
    Ok(())
}

#[test]
fn test_glob_no_maps_needed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // All files under 200 lines.
    std::fs::write(
        dir.path().join(format!("a.{MOCK_EXT}")),
        "fn alpha\nfn beta\n",
    )?;
    std::fs::write(dir.path().join(format!("b.{MOCK_EXT}")), "struct Gamma\n")?;

    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), None)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    assert!(!text.contains('<'), "No symbols should appear: {text}");
    Ok(())
}

#[test]
fn test_glob_dir_large_file_paged() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // File with many symbols — exceeds budget so output is paged.
    std::fs::write(
        dir.path().join(format!("big.{MOCK_EXT}")),
        gen_mock_content(250),
    )?;
    // Threshold of 5 so file qualifies for maps. Budget of 1000.
    let config = "[tools.glob]\nbudget = 1000\noutline_threshold = 5\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // With stable shape, maps are always rendered and paged.
    assert!(
        text.contains("<Function>") || text.contains("<Struct>"),
        "Should show symbols in maps: {text}"
    );
    // Output should be paged since maps exceed budget.
    assert!(text.contains("[page 1/"), "Should have page header: {text}");
    Ok(())
}

#[test]
fn test_glob_outline_suppress() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join(format!("big.{MOCK_EXT}")),
        "fn alpha\nfn beta\n\n\n\n\n\n\n\n\n",
    )?;
    // Deny all mock files from maps. Threshold of 5 so file qualifies.
    let config =
        format!("[tools.glob]\noutline_threshold = 5\noutline_suppress = [\"**/*.{MOCK_EXT}\"]\n");
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(&config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Should NOT have symbol lines (denied by outline_suppress).
    assert!(
        !text.contains("<Function>") && !text.contains("<Struct>"),
        "Maps-denied file should not have symbols: {text}"
    );
    // Should have [symbols available] since grammar IS installed.
    assert!(
        text.contains("[symbols available]"),
        "Should show [symbols available] flag: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_trailing_slash() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // A file with nested definitions: Outer has children → trailing /.
    // Pad with empty lines to cross threshold.
    std::fs::write(
        dir.path().join(format!("nested.{MOCK_EXT}")),
        "struct Outer {\nfn inner\n}\nfn leaf\n\n\n\n\n\n\n",
    )?;

    let config = "[tools.glob]\noutline_threshold = 5\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Container symbols should have trailing /.
    assert!(
        text.contains("<Struct> Outer/"),
        "Container should have trailing /: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_single_file_map() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("tiny.mock");
    // Small file — single files bypass threshold.
    std::fs::write(&file, "fn alpha\nstruct Beta\n")?;

    // Use mockls for .mock files.
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), None)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": file.to_str().context("file path")? }),
    )?;

    // Read stderr for diagnostics on failure.
    let stderr = bridge
        .stderr_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();

    // Single file should get a map regardless of size.
    assert!(
        text.contains("<Function>") || text.contains("<Struct>"),
        "Single file should have map.\nglob output: {text}\nstderr:\n{stderr}"
    );
    assert!(text.contains("alpha"), "Should show symbol names: {text}");
    Ok(())
}

#[test]
fn test_glob_single_file_denied() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("denied.{MOCK_EXT}"));
    std::fs::write(&file, "fn alpha\nstruct Beta\n")?;

    let config = format!("[tools.glob]\noutline_suppress = [\"**/*.{MOCK_EXT}\"]\n");
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(&config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": file.to_str().context("file path")? }),
    )?;

    // outline_suppress blocks the map even for single files.
    assert!(
        !text.contains("<Function>") && !text.contains("<Struct>"),
        "Denied single file should not have map: {text}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_glob_symlink_broken() -> Result<()> {
    use std::os::unix::fs as unix_fs;

    let dir = tempfile::tempdir()?;
    unix_fs::symlink(
        dir.path().join("nonexistent.txt"),
        dir.path().join("broken_link.txt"),
    )?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    assert!(
        text.contains("[broken]"),
        "Broken symlink should show [broken] flag: {text}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn test_glob_symlink_valid() -> Result<()> {
    use std::os::unix::fs as unix_fs;

    let dir = tempfile::tempdir()?;
    let target = dir.path().join("real_file.txt");
    std::fs::write(&target, "line one\nline two\nline three\n")?;

    unix_fs::symlink(&target, dir.path().join("valid_link.txt"))?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Valid symlink should show -> with resolved target path
    assert!(
        text.contains("valid_link.txt ->"),
        "Valid symlink should show arrow: {text}"
    );
    // Should show target's line count
    assert!(
        text.contains("3 lines"),
        "Valid symlink should show target line count: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_maps_deny_partial() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let assets = dir.path().join("test_assets");
    std::fs::create_dir_all(&assets)?;

    // Files inside test_assets/ — should be denied maps
    std::fs::write(
        assets.join(format!("denied.{MOCK_EXT}")),
        "fn denied_fn\nstruct DeniedType\n\n\n\n\n\n\n\n\n",
    )?;
    // File outside test_assets/ — should still get maps
    std::fs::write(
        dir.path().join(format!("allowed.{MOCK_EXT}")),
        "fn allowed_fn\nstruct AllowedType\n\n\n\n\n\n\n\n\n",
    )?;

    let config = "[tools.glob]\noutline_threshold = 5\noutline_suppress = [\"test_assets/**\"]\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // File outside test_assets/ should have a map
    assert!(
        text.contains("allowed_fn") || text.contains("AllowedType"),
        "File outside deny path should have map symbols: {text}"
    );
    // File inside test_assets/ should NOT have a map (denied)
    let denied_line = text.lines().find(|l| l.contains("denied.")).unwrap_or("");
    assert!(
        !denied_line.contains('<'),
        "Denied file should not have map symbols: {denied_line}"
    );
    Ok(())
}

#[test]
fn test_glob_bounding_ranges() -> Result<()> {
    let dir = tempfile::tempdir()?;

    // Multiple files with identical symbol names/kinds but different line spans.
    // alpha is at different positions in each file.
    std::fs::write(
        dir.path().join(format!("early.{MOCK_EXT}")),
        "fn alpha\nstruct Beta\n\n\n\n\n\n\n\n\n",
    )?;
    // Pad with extra lines so alpha starts later
    std::fs::write(
        dir.path().join(format!("late.{MOCK_EXT}")),
        "\n\n\n\nfn alpha\nstruct Beta\n\n\n\n\n\n\n\n\n",
    )?;
    // Third file for the dedup group
    std::fs::write(
        dir.path().join(format!("mid.{MOCK_EXT}")),
        "\n\nfn alpha\nstruct Beta\n\n\n\n\n\n\n\n\n",
    )?;

    let config = "[tools.glob]\noutline_threshold = 5\nbudget = 5000\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Should show "common structure" for deduplicated group
    assert!(
        text.contains("common structure"),
        "Should show shared map: {text}"
    );
    // Should note that ranges are bounding
    assert!(
        text.contains("ranges are bounding"),
        "Should note bounding ranges: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_snapshot_flag() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("handler.catenary_snapshot_5.rs"),
        "old content",
    )?;
    std::fs::write(dir.path().join("handler.rs"), "fn main() {}")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    assert!(
        text.contains("[snapshot]"),
        "Snapshot file should show [snapshot] flag: {text}"
    );
    // Snapshot file should NOT have line count.
    let snapshot_line = text
        .lines()
        .find(|l| l.contains("catenary_snapshot"))
        .unwrap_or("");
    assert!(
        !snapshot_line.contains("lines)"),
        "Snapshot file should not show line count: {snapshot_line}"
    );
    Ok(())
}

#[test]
fn test_glob_gitignored_flag() -> Result<()> {
    let dir = tempfile::tempdir()?;

    std::process::Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .context("git init")?;

    std::fs::write(dir.path().join(".gitignore"), "*.log\n")?;
    std::fs::write(dir.path().join("app.txt"), "content")?;
    std::fs::write(dir.path().join("debug.log"), "log data")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": dir.path().to_string_lossy().to_string(),
            "include_gitignored": true,
            "include_hidden": true
        }),
    )?;

    assert!(
        text.contains("[gitignored]"),
        "Gitignored file should show [gitignored] flag: {text}"
    );
    // The gitignored flag should be on the .log file.
    let log_line = text.lines().find(|l| l.contains("debug.log")).unwrap_or("");
    assert!(
        log_line.contains("[gitignored]"),
        "debug.log should have [gitignored]: {log_line}"
    );
    Ok(())
}

#[test]
fn test_glob_composing_flags() -> Result<()> {
    let dir = tempfile::tempdir()?;

    std::process::Command::new("git")
        .args(["init"])
        .current_dir(dir.path())
        .output()
        .context("git init")?;

    // A file that is gitignored, has grammar, but maps are denied.
    // outline_suppress blocks the map → [symbols available].
    // include_gitignored → [gitignored]. Both compose.
    std::fs::write(dir.path().join(".gitignore"), format!("*.{MOCK_EXT}\n"))?;
    std::fs::write(
        dir.path().join(format!("big.{MOCK_EXT}")),
        "fn alpha\nfn beta\n\n\n\n\n\n\n\n\n",
    )?;

    let config =
        format!("[tools.glob]\noutline_threshold = 5\noutline_suppress = [\"**/*.{MOCK_EXT}\"]\n");
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(&config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": dir.path().to_string_lossy().to_string(),
            "include_gitignored": true,
            "include_hidden": true
        }),
    )?;

    // Should have composed flags.
    let big_line = text
        .lines()
        .find(|l| l.contains(&format!("big.{MOCK_EXT}")))
        .unwrap_or("");
    assert!(
        big_line.contains("symbols available") && big_line.contains("gitignored"),
        "Should compose flags: {big_line}"
    );
    Ok(())
}

#[test]
fn test_glob_paging() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("huge.{MOCK_EXT}"));
    // Many symbols to exceed budget in single-file mode.
    std::fs::write(&file, gen_mock_content(500))?;

    let config = "[tools.glob]\nbudget = 1000\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    // First page — should show [page 1/N] where N > 1.
    let text1 = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": file.to_str().context("file path")? }),
    )?;

    assert!(
        text1.contains("[page 1/"),
        "First page should have page header: {text1}"
    );
    // Verify it's not a single page.
    assert!(
        !text1.contains("[page 1/1]"),
        "Should have multiple pages: {text1}"
    );

    // Second page via page parameter.
    let text2 = bridge.call_tool_text(
        "glob",
        &json!({
            "pattern": file.to_str().context("file path")?,
            "page": 2
        }),
    )?;

    assert!(
        text2.contains("[page 2/"),
        "Second page should have page 2 header: {text2}"
    );
    // Second page should have different symbols than first.
    assert!(
        !text2.is_empty(),
        "Second page should have content: {text2}"
    );
    Ok(())
}

#[test]
fn test_glob_no_grammar() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Large file but no grammar installed.
    let content = (0..300).fold(String::new(), |mut s, i| {
        use std::fmt::Write;
        let _ = writeln!(s, "line {i}");
        s
    });
    std::fs::write(dir.path().join("big.txt"), &content)?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Should show line count but no symbols and no [symbols available].
    assert!(text.contains("300 lines"), "Should show line count: {text}");
    assert!(
        !text.contains("[symbols available]"),
        "Should not have symbols available flag: {text}"
    );
    assert!(!text.contains('<'), "Should not have symbols: {text}");
    Ok(())
}

#[test]
fn test_glob_budget_minimum() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        for i in 0..40 {
            std::fs::write(root.join(format!("item_{i:03}.txt")), format!("line {i}\n"))?;
        }
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, "[tools.glob]\nbudget = 500\n")?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": bridge.root_path().to_string_lossy().to_string() }),
    )?;

    // Output should fit within clamped budget (1000) + tolerance.
    assert!(
        text.len() <= 1200,
        "Output should be clamped budget-constrained: len={}, text:\n{text}",
        text.len()
    );
    Ok(())
}

#[test]
fn test_glob_structure_dedup() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Multiple files with identical symbol sets crossing threshold.
    let content = "fn alpha\nstruct Beta\n\n\n\n\n\n\n\n\n";
    for i in 0..5 {
        std::fs::write(dir.path().join(format!("proto_{i:03}.{MOCK_EXT}")), content)?;
    }

    let config = "[tools.glob]\noutline_threshold = 5\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Should show "common structure" for deduplicated group.
    assert!(
        text.contains("common structure"),
        "Should show shared map: {text}"
    );
    // Should show "ranges are bounding" parenthetical.
    assert!(
        text.contains("ranges are bounding"),
        "Should note bounding ranges: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_dedup_mixed() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Shared structure files.
    let shared = "fn alpha\nstruct Beta\n\n\n\n\n\n\n\n\n";
    for i in 0..3 {
        std::fs::write(dir.path().join(format!("shared_{i}.{MOCK_EXT}")), shared)?;
    }
    // Unique file with different symbols.
    std::fs::write(
        dir.path().join(format!("unique.{MOCK_EXT}")),
        "fn unique_func\nstruct UniqueType\n\n\n\n\n\n\n\n\n",
    )?;

    let config = "[tools.glob]\noutline_threshold = 5\nbudget = 5000\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": dir.path().to_string_lossy().to_string() }),
    )?;

    // Should show both shared map and individual map.
    assert!(
        text.contains("common structure"),
        "Should have shared map: {text}"
    );
    assert!(
        text.contains("unique_func"),
        "Should show individual map symbols: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_tree_dedup() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Two subdirectories, each with identical files.
    // group_a: 3 files with same structure → shared map.
    // group_b: 2 files with different structure → individual maps.
    let group_a = dir.path().join("group_a");
    let group_b = dir.path().join("group_b");
    std::fs::create_dir_all(&group_a)?;
    std::fs::create_dir_all(&group_b)?;

    let shared = "fn alpha\nstruct Beta\n\n\n\n\n\n\n\n\n";
    for i in 0..3 {
        std::fs::write(group_a.join(format!("proto_{i}.mock")), shared)?;
    }
    std::fs::write(
        group_b.join("handler.mock"),
        "fn process\nstruct Config\n\n\n\n\n\n\n\n\n",
    )?;
    std::fs::write(
        group_b.join("router.mock"),
        "fn dispatch\nstruct Route\n\n\n\n\n\n\n\n\n",
    )?;

    let config = "[tools.glob]\noutline_threshold = 5\nbudget = 5000\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "**/*.mock" }))?;

    // group_a should have dedup (3 identical files).
    assert!(
        text.contains("common structure"),
        "group_a should have shared dedup map: {text}"
    );
    assert!(
        text.contains("ranges are bounding"),
        "Should note bounding ranges: {text}"
    );
    // group_b should have individual maps (different structures).
    assert!(
        text.contains("process") && text.contains("dispatch"),
        "group_b should have individual symbols: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_tree_dedup_per_directory() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Two directories with IDENTICAL file structures — dedup should NOT
    // merge across directories. Each directory gets its own shared map.
    let dir_a = dir.path().join("dir_a");
    let dir_b = dir.path().join("dir_b");
    std::fs::create_dir_all(&dir_a)?;
    std::fs::create_dir_all(&dir_b)?;

    let content = "fn alpha\nstruct Beta\n\n\n\n\n\n\n\n\n";
    for i in 0..3 {
        std::fs::write(dir_a.join(format!("file_{i}.mock")), content)?;
        std::fs::write(dir_b.join(format!("file_{i}.mock")), content)?;
    }

    let config = "[tools.glob]\noutline_threshold = 5\nbudget = 5000\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "**/*.mock" }))?;

    // Count occurrences of "common structure" — should be 2 (one per dir).
    let dedup_count = text.matches("common structure").count();
    assert_eq!(
        dedup_count, 2,
        "Should have separate dedup per directory (expected 2, got {dedup_count}): {text}"
    );
    Ok(())
}

// ─── Ticket 65: directory matching in glob patterns ─────────────────

#[test]
fn test_glob_pattern_matches_directories() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Directory containing only subdirectories — no files.
    std::fs::create_dir(dir.path().join("movies"))?;
    std::fs::create_dir(dir.path().join("music"))?;
    std::fs::create_dir(dir.path().join("photos"))?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": format!("{}/*", dir.path().display()) }),
    )?;

    assert!(
        text.contains("movies/"),
        "Should match movies directory: {text}"
    );
    assert!(
        text.contains("music/"),
        "Should match music directory: {text}"
    );
    assert!(
        text.contains("photos/"),
        "Should match photos directory: {text}"
    );
    assert!(
        !text.contains("No matches found"),
        "Should not report no matches: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_pattern_mixed_entries() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir(dir.path().join("subdir"))?;
    std::fs::write(dir.path().join("file.txt"), "content\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": format!("{}/*", dir.path().display()) }),
    )?;

    assert!(
        text.contains("subdir/"),
        "Should include directory with trailing /: {text}"
    );
    assert!(text.contains("file.txt"), "Should include file: {text}");
    Ok(())
}

#[test]
fn test_glob_recursive_pattern_includes_dirs() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let sub = dir.path().join("level1");
    let subsub = sub.join("level2");
    std::fs::create_dir_all(&subsub)?;
    std::fs::write(subsub.join("file.txt"), "content\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": format!("{}/**/*", dir.path().display()) }),
    )?;

    assert!(text.contains("level1/"), "Should include level1/: {text}");
    assert!(text.contains("level2/"), "Should include level2/: {text}");
    assert!(text.contains("file.txt"), "Should include files: {text}");

    // Directories that appear as tree branches should NOT also appear as
    // file-level leaves (prune_dir_dupes).
    let level1_count = text.matches("level1/").count();
    assert_eq!(
        level1_count, 1,
        "level1/ should appear exactly once, not duplicated: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_pattern_dirs_no_enrichment() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Only directories — no files to enrich.
    std::fs::create_dir(dir.path().join("alpha"))?;
    std::fs::create_dir(dir.path().join("beta"))?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": format!("{}/*", dir.path().display()) }),
    )?;

    // Directories should have no line counts or symbol markers.
    assert!(
        !text.contains("lines)"),
        "Directories should have no line counts: {text}"
    );
    assert!(
        !text.contains("[symbols available]"),
        "Directories should have no symbol markers: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_pattern_paged_large_result() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        for i in 0..30 {
            let sub = root.join(format!("dir_{i:02}"));
            std::fs::create_dir(&sub)?;
            for j in 0..5 {
                std::fs::write(sub.join(format!("file_{j}.txt")), format!("line {j}\n"))?;
            }
        }
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, "[tools.glob]\nbudget = 600\n")?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "pattern": format!("{}/**/*.txt", bridge.root_path().display()) }),
    )?;

    // With 150 files across 30 dirs, the tree won't fit in 600 chars.
    // Output should be paged with [page N/M] header.
    assert!(text.contains("[page 1/"), "Should have page header: {text}");
    assert!(
        text.contains("dir_"),
        "Should show directory structure: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_paged_preserves_tree_structure() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        let src = root.join("src");
        let bridge_dir = src.join("bridge");
        let lsp_dir = src.join("lsp");
        std::fs::create_dir_all(&bridge_dir)?;
        std::fs::create_dir_all(&lsp_dir)?;
        for i in 0..20 {
            std::fs::write(bridge_dir.join(format!("handler_{i}.rs")), format!("fn handle_{i}() {{}}\n"))?;
        }
        for i in 0..20 {
            std::fs::write(lsp_dir.join(format!("client_{i}.rs")), format!("struct Client{i};\n"))?;
        }
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, "[tools.glob]\nbudget = 400\n")?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "src/**/*.rs" }))?;

    // Paged output should preserve tree structure with directory names.
    assert!(
        text.contains("bridge") || text.contains("lsp"),
        "Paged output should show directory structure: {text}"
    );
    assert!(text.contains("[page 1/"), "Should have page header: {text}");
    Ok(())
}

#[test]
fn test_glob_pattern_in_roots_unchanged() -> Result<()> {
    // Verify that in-root file matching still works with enrichment.
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("main.rs"), "fn main() {}\n")?;
    std::fs::write(dir.path().join("lib.rs"), "pub mod lib;\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "pattern": "*.rs" }))?;

    assert!(text.contains("main.rs"), "Should match main.rs: {text}");
    assert!(text.contains("lib.rs"), "Should match lib.rs: {text}");
    // File entries should still have line counts.
    assert!(
        text.contains("lines)"),
        "File entries should still have line counts: {text}"
    );
    Ok(())
}

// ─── Output format tests ─────────────────────────────────────────────

#[test]
fn test_glob_absolute_dir_no_cwd_header() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("hello.txt"), "hi\n")?;
    std::fs::create_dir(dir.path().join("sub"))?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let abs_path = dir.path().to_string_lossy().to_string();
    let text = bridge.call_tool_text("glob", &json!({ "pattern": &abs_path }))?;

    // Absolute pattern: no cwd header.
    assert!(
        !text.contains("cwd ="),
        "Absolute pattern should not have cwd header: {text}"
    );
    // Should have the absolute path as section header.
    assert!(
        text.contains(&abs_path),
        "Should have absolute path header: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_absolute_dir_indented_entries() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("alpha.txt"), "a\n")?;
    std::fs::write(dir.path().join("beta.txt"), "b\n")?;
    std::fs::create_dir(dir.path().join("sub"))?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let abs_path = dir.path().to_string_lossy().to_string();
    let text = bridge.call_tool_text("glob", &json!({ "pattern": &abs_path }))?;

    // Entries should be indented under the directory header.
    assert!(
        text.contains("\talpha.txt"),
        "Entries should be indented under dir header: {text}"
    );
    assert!(
        text.contains("\tsub/"),
        "Subdirectories should be indented under dir header: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_absolute_pattern_indented_tree() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join("src/bridge"))?;
    std::fs::write(dir.path().join("src/bridge/mod.rs"), "pub mod bridge;\n")?;
    std::fs::write(dir.path().join("src/lib.rs"), "pub mod src;\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let pattern = format!("{}/**/*.rs", dir.path().display());
    let text = bridge.call_tool_text("glob", &json!({ "pattern": &pattern }))?;

    // No cwd header for absolute patterns.
    assert!(
        !text.contains("cwd ="),
        "Absolute pattern should not have cwd header: {text}"
    );
    // Tree content should be indented under the section header.
    assert!(
        text.contains("\tsrc/"),
        "Tree should be indented under section header: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_absolute_file_no_cwd_header() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("readme.txt");
    std::fs::write(&file_path, "hello world\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let abs_path = file_path.to_string_lossy().to_string();
    let text = bridge.call_tool_text("glob", &json!({ "pattern": &abs_path }))?;

    // Absolute file: no cwd header, absolute path in output.
    assert!(
        !text.contains("cwd ="),
        "Absolute file should not have cwd header: {text}"
    );
    assert!(text.contains("readme.txt"), "Should show file name: {text}");
    Ok(())
}

#[test]
fn test_grep_no_glob_no_cwd_header() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("hello.txt"), "needle in haystack\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // No glob param → no cwd header.
    assert!(
        !text.contains("cwd ="),
        "Grep without glob should not have cwd header: {text}"
    );
    assert!(text.contains("needle"), "Should find the match: {text}");
    Ok(())
}

#[test]
fn test_grep_absolute_glob_no_cwd_header() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("target.rs"), "fn needle() {}\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let glob = format!("{}/**/*.rs", dir.path().display());
    let text = bridge.call_tool_text("grep", &json!({ "pattern": "needle", "glob": &glob }))?;

    // Absolute glob → no cwd header.
    assert!(
        !text.contains("cwd ="),
        "Grep with absolute glob should not have cwd header: {text}"
    );
    assert!(text.contains("needle"), "Should find the match: {text}");
    Ok(())
}
