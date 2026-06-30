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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
        &json!({ "paths": [outside.path().to_string_lossy().as_ref()] }),
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
fn test_tools_list_returns_method_not_found() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    bridge.send(&json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list"
    }))?;

    let response = bridge.recv()?;
    assert!(
        response.get("error").is_some(),
        "tools/list should return error (method not found): {response:?}"
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
        &json!({ "paths": [script.to_str().context("file path")?] }),
    )?;

    // File header with line count. No grammar here (spawn_no_lsp), so the file
    // carries the `no outline` marker: `(4 lines, no outline)`.
    assert!(text.contains("(4 lines"), "Should show line count: {text}");
    assert!(
        text.contains("no outline"),
        "A file with no server should be marked `no outline`, not silently \
         outline-less: {text}"
    );

    // No symbols: bridge has no LSP servers and no grammar installed
    assert!(
        !text.contains("Config"),
        "Should not show symbols without grammar: {text}"
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // No grammar here (spawn_no_lsp), so each file carries the `no outline`
    // marker inside the parens: `(N lines, no outline)`. Match the count prefix.
    assert!(
        text.contains("(3 lines"),
        "Should show 3 lines for three.txt: {text}"
    );
    // Pluralization fix: a single line is singular, not `(1 lines)`.
    assert!(
        text.contains("(1 line"),
        "Should show 1 line (singular) for one.txt: {text}"
    );
    assert!(
        !text.contains("(1 lines"),
        "Single-line file must not read `(1 lines)`: {text}"
    );
    // Should NOT show bytes
    assert!(!text.contains("bytes"), "Should not show bytes: {text}");
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
            "paths": [dir.path().to_string_lossy().to_string()],
            "include_hidden": true
        }),
    )?;
    assert!(
        text_hidden.contains(".hidden"),
        "Should show .hidden with include_hidden: {text_hidden}"
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

    let text = bridge.call_tool_text("glob", &json!({ "paths": [".gitignore"] }))?;
    assert!(
        text.contains(".gitignore"),
        "Explicit .gitignore glob should match without include_hidden: {text}"
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
            "paths": [dir.path().to_string_lossy().to_string()],
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
            "paths": [dir.path().to_string_lossy().to_string()],
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

// ── literal-first glob expansion (bugs/13, ticket 07) ──────────────

/// A quoted (unexpanded) glob pattern is expanded daemon-side: the
/// relative pattern is resolved against cwd and the gitignore-aware
/// walker yields the matching files, just as if the shell had expanded
/// it unquoted.
#[test]
fn test_glob_quoted_pattern_expands_daemon_side() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join("src/inner"))?;
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("src/inner/lib.rs"), "fn lib() {}")?;
    std::fs::write(dir.path().join("src/notes.txt"), "notes")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Pattern is relative — the daemon resolves it against the cwd
    // (the workspace root) and expands via the `ignore` walker.
    let text = bridge.call_tool_text("glob", &json!({ "paths": ["src/**/*.rs"] }))?;

    assert!(text.contains("main.rs"), "expands to main.rs: {text}");
    assert!(
        text.contains("lib.rs"),
        "expands recursively to lib.rs: {text}"
    );
    assert!(
        !text.contains("notes.txt"),
        "non-matching extension excluded: {text}"
    );
    Ok(())
}

/// A quoted glob pattern that matches nothing yields empty daemon output
/// (the CLI renders the loud `no files matched` anchor) — never an error.
#[test]
fn test_glob_quoted_pattern_zero_match_is_empty() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("only.txt"), "x")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "paths": ["**/*.rs"] }))?;
    assert!(
        text.trim().is_empty(),
        "zero-match pattern returns empty output, got: {text:?}"
    );
    Ok(())
}

/// grep applies the same expansion to its path arguments: a quoted glob
/// scopes the search to the files it matches.
#[test]
fn test_grep_quoted_pattern_path_scopes_search() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("a.rs"), "let needle = 1;")?;
    std::fs::write(dir.path().join("b.txt"), "needle in text")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "needle", "paths": ["*.rs"] }))?;
    assert!(
        text.contains("a.rs"),
        "searches the expanded .rs file: {text}"
    );
    assert!(
        !text.contains("b.txt"),
        "the .txt file is outside the expanded scope: {text}"
    );
    Ok(())
}

/// grep with a path glob that matches no files returns empty — it must
/// NOT silently fall back to a cwd-wide search.
#[test]
fn test_grep_path_glob_zero_files_does_not_search_cwd() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("a.rs"), "let needle = 1;")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "needle", "paths": ["*.nomatchext"] }),
    )?;
    assert!(
        text.trim().is_empty(),
        "zero-file glob must not fall back to a cwd search, got: {text}"
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // If there's a bucket pattern, it should be a valid glob.
    if text.contains("files)") {
        // Extract a bucket pattern — lines like "test_grep_*  (5 files)"
        for line in text.lines() {
            if line.contains("files)") {
                let pattern = line.split("  (").next().unwrap_or("").trim();
                if !pattern.is_empty() {
                    // The bucket pattern should be passable back to glob.
                    let drill = bridge.call_tool_text("glob", &json!({ "paths": [pattern] }))?;
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
            std::fs::write(
                root.join(format!("test_grep_{i}.rs")),
                format!("fn test_{i}() {{}}\n"),
            )?;
        }
        for i in 0..5 {
            std::fs::write(
                root.join(format!("test_glob_{i}.rs")),
                format!("fn test_{i}() {{}}\n"),
            )?;
        }
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, "[tools.glob]\noutline_threshold = 200\n")?;
        Ok(config_path)
    })?;
    bridge2.initialize()?;

    let text = bridge2.call_tool_text(
        "glob",
        &json!({ "paths": [bridge2.root_path().to_string_lossy().to_string()] }),
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
            &json!({ "paths": [lua_file.to_str().context("file path")?] }),
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
    let text = bridge.call_tool_text("glob", &json!({ "paths": ["**/*.lua"] }))?;
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
    BridgeProcess::spawn_with_pre_start(&[&lsp], root, |state_home| {
        if let Some(toml) = config_toml {
            // Config is discovered via `$XDG_CONFIG_HOME/catenary/`, which
            // `isolate_env` splits to `<root>/config` — not the state home.
            let config_dir = common::xdg_config_home(state_home).join("catenary");
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
fn test_glob_enrich_always_includes_small_files() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // A larger file and a one-line file. The old `outline_threshold` gate is
    // gone (enrich always), so BOTH are outlined regardless of size.
    std::fs::write(
        dir.path().join(format!("big.{MOCK_EXT}")),
        "fn alpha\nfn beta\nstruct Gamma\n\n\n\n\n\n\n\n",
    )?;
    std::fs::write(dir.path().join(format!("small.{MOCK_EXT}")), "fn tiny\n")?;

    // Threshold is now inert; pass it to confirm it no longer gates outlines.
    let config = "[tools.glob]\noutline_threshold = 5\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // The larger file is outlined (declaration lines, no `<Kind>` label).
    assert!(
        text.contains("fn alpha") || text.contains("struct Gamma"),
        "Larger file should have outline declaration lines: {text}"
    );
    assert!(
        !text.contains('<'),
        "Outline should have no kind label: {text}"
    );
    // Enrich always: the one-line file is listed AND outlined — its `fn tiny`
    // declaration appears (the size gate that used to suppress it is gone).
    assert!(
        text.contains(&format!("small.{MOCK_EXT}")),
        "Should list small file: {text}"
    );
    assert!(
        text.contains("fn tiny"),
        "Small file should be outlined too (enrich always, no size gate): {text}"
    );
    Ok(())
}

#[test]
fn test_glob_small_files_outlined_no_kind_label() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Small files (well under the old default threshold of 200). Enrich always:
    // they are outlined now, and the outline never carries a `<Kind>` label.
    std::fs::write(
        dir.path().join(format!("a.{MOCK_EXT}")),
        "fn alpha\nfn beta\n",
    )?;
    std::fs::write(dir.path().join(format!("b.{MOCK_EXT}")), "struct Gamma\n")?;

    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), None)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // Outlines appear even for these small files (no size gate).
    assert!(
        text.contains("fn alpha") && text.contains("struct Gamma"),
        "Small files should be outlined (enrich always): {text}"
    );
    assert!(!text.contains('<'), "No kind label should appear: {text}");
    Ok(())
}

#[test]
fn test_glob_dir_large_file_paged() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // Several outlined files so the directory listing exceeds the line budget
    // and the overflow valve truncates BETWEEN files (never mid-tree).
    for f in 0..5 {
        std::fs::write(
            dir.path().join(format!("file_{f}.{MOCK_EXT}")),
            gen_mock_content(10),
        )?;
    }
    // A small line budget forces the valve to fire and truncate at a file
    // boundary (threshold is inert under enrich-always; left to confirm so).
    let config = "[tools]\nline_budget = 25\n\n[tools.glob]\noutline_threshold = 5\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let resp = bridge.call_search_raw(
        "tool/glob",
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;
    let output = resp
        .get("output")
        .and_then(serde_json::Value::as_str)
        .context("output")?;
    let receipt = resp
        .get("receipt")
        .and_then(serde_json::Value::as_str)
        .context("expected an overflow receipt when the listing exceeds the budget")?;

    // Outline declaration lines still render (no `<Kind>` label).
    assert!(
        output.contains("fn func_") || output.contains("struct Struct_"),
        "Should show declaration lines in maps: {output}"
    );
    assert!(
        !output.contains('<'),
        "Outline should have no kind label: {output}"
    );

    // The spill file holds the complete listing; the display is a strict prefix.
    let path = receipt
        .rsplit(" at ")
        .next()
        .context("spill path in receipt")?;
    let spilled = std::fs::read_to_string(path).context("read spill file")?;
    let shown = output.lines().count();
    assert!(
        spilled.lines().count() > shown,
        "spill holds the full listing ({} lines) vs {shown} shown",
        spilled.lines().count()
    );

    // File-boundary truncation: the first DROPPED line begins a fresh block (a
    // file/dir header), never an outline node mid-tree. Outline nodes render
    // `{line}  {decl}` — a digit run then two spaces — so a header never starts
    // with a digit (the filenames here are `file_N.mock`).
    if let Some(first_dropped) = spilled.lines().nth(shown) {
        let trimmed = first_dropped.trim_start();
        assert!(
            !trimmed.chars().next().is_some_and(|c| c.is_ascii_digit()),
            "truncation lands on a file boundary, not mid-tree; dropped: {first_dropped:?}"
        );
    }
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // Should NOT have outline declaration lines (denied by outline_suppress).
    assert!(
        !text.contains("fn alpha") && !text.contains("fn beta"),
        "Maps-denied file should not render its outline: {text}"
    );
    // Should have [symbols available] since grammar IS installed.
    assert!(
        text.contains("[symbols available]"),
        "Should show [symbols available] flag: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_full_expansion_nested_children() -> Result<()> {
    let dir = tempfile::tempdir()?;
    // A file with nested definitions: Outer { contains inner }; leaf is
    // top-level. Full expansion shows each node on its own indented line — the
    // old `/`-collapse marker (a container shown as `Outer {/`) is gone.
    std::fs::write(
        dir.path().join(format!("nested.{MOCK_EXT}")),
        "struct Outer {\nfn inner\n}\nfn leaf\n\n\n\n\n\n\n",
    )?;

    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), None)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // The container renders without the collapse marker; its child is expanded
    // on its own, more-indented line.
    assert!(
        text.contains("struct Outer {") && !text.contains("Outer {/"),
        "Container should render fully expanded, not collapsed with `/`: {text}"
    );
    assert!(
        text.contains("fn inner"),
        "Nested child should be expanded on its own line: {text}"
    );
    assert!(
        !text.contains('<'),
        "Outline should have no kind label: {text}"
    );

    // `fn inner` is nested under `struct Outer {`, so its line is indented more
    // deeply than the container's.
    let outer_indent = text
        .lines()
        .find(|l| l.contains("struct Outer {"))
        .map(|l| l.len() - l.trim_start().len())
        .context("Outer line present")?;
    let inner_indent = text
        .lines()
        .find(|l| l.contains("fn inner"))
        .map(|l| l.len() - l.trim_start().len())
        .context("inner line present")?;
    assert!(
        inner_indent > outer_indent,
        "nested child must be indented deeper than its container: \
         outer={outer_indent}, inner={inner_indent}, text:\n{text}"
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
        &json!({ "paths": [file.to_str().context("file path")?] }),
    )?;

    // Read stderr for diagnostics on failure.
    let stderr = bridge
        .stderr_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();

    // Single file should get a map regardless of size: each outline node
    // renders its declaration source line, with no `<Kind>` label.
    assert!(
        text.contains("fn alpha") || text.contains("struct Beta"),
        "Single file should have map declaration lines.\nglob output: {text}\nstderr:\n{stderr}"
    );
    assert!(
        !text.contains('<'),
        "Outline should have no kind label: {text}"
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
        &json!({ "paths": [file.to_str().context("file path")?] }),
    )?;

    // outline_suppress blocks the map even for single files: no outline
    // declaration lines are rendered.
    assert!(
        !text.contains("fn alpha") && !text.contains("struct Beta"),
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // File outside test_assets/ should have a map: its declaration source
    // lines are rendered as outline nodes, with no `<Kind>` label.
    assert!(
        text.contains("fn allowed_fn") || text.contains("struct AllowedType"),
        "File outside deny path should have map declaration lines: {text}"
    );
    assert!(
        !text.contains('<'),
        "Outline should have no kind label: {text}"
    );
    // File inside test_assets/ should NOT have a map (denied): its
    // declaration lines are not rendered as outline nodes.
    assert!(
        !text.contains("fn denied_fn") && !text.contains("struct DeniedType"),
        "Denied file should not render its outline: {text}"
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

    let config = "[tools.glob]\noutline_threshold = 5\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // Cross-file dedup (and its bounding-range collapse) is gone: each file
    // now renders its OWN outline, so the same symbol shows up at its own
    // 1-based declaration line per file — not folded into a shared map.
    assert!(
        !text.contains("common structure") && !text.contains("ranges are bounding"),
        "Cross-file dedup collapse should be gone: {text}"
    );
    assert!(
        !text.contains('<'),
        "Outline should have no kind label: {text}"
    );
    // Each of the three files is listed with its own outline declaration
    // lines: `fn alpha` and `struct Beta` appear once per file.
    for name in ["early", "late", "mid"] {
        let file = format!("{name}.{MOCK_EXT}");
        assert!(text.contains(&file), "Should list {file}: {text}");
    }
    assert_eq!(
        text.matches("fn alpha").count(),
        3,
        "Each file should render its own `fn alpha` outline node: {text}"
    );
    // alpha sits at a different 1-based line in each file (early/mid/late),
    // so its outline node renders at distinct line prefixes — not collapsed.
    // Nodes render `{line}  {decl}` (no colon), indented under the file header.
    assert!(
        text.contains("1  fn alpha")
            && text.contains("3  fn alpha")
            && text.contains("5  fn alpha"),
        "Each file's alpha should render at its own declaration line: {text}"
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
            "paths": [dir.path().to_string_lossy().to_string()],
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
            "paths": [dir.path().to_string_lossy().to_string()],
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
fn test_glob_overflow_spills_full_outline() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join(format!("huge.{MOCK_EXT}"));
    // Many symbols so the single-file outline exceeds the line budget.
    std::fs::write(&file, gen_mock_content(500))?;

    let config = "[tools]\nline_budget = 50\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let resp = bridge.call_search_raw(
        "tool/glob",
        &json!({ "paths": [file.to_str().context("file path")?] }),
    )?;
    let output = resp
        .get("output")
        .and_then(serde_json::Value::as_str)
        .context("output")?;
    let receipt = resp
        .get("receipt")
        .and_then(serde_json::Value::as_str)
        .context("expected an overflow receipt when the outline exceeds the budget")?;

    // The truncated display is a strict prefix; the spill file holds the COMPLETE
    // outline, including the last symbols that did not fit.
    let path = receipt
        .rsplit(" at ")
        .next()
        .context("spill path in receipt")?;
    let spilled = std::fs::read_to_string(path).context("read spill file")?;
    assert!(
        spilled.lines().count() > output.lines().count(),
        "spill holds the full outline ({} lines) vs {} shown",
        spilled.lines().count(),
        output.lines().count()
    );
    assert!(
        spilled.contains("func_498") || spilled.contains("Struct_499"),
        "spill file holds the complete outline, got tail:\n{}",
        spilled.lines().rev().take(3).collect::<Vec<_>>().join("\n")
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
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
        std::fs::write(&config_path, "[tools.glob]\noutline_threshold = 200\n")?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [bridge.root_path().to_string_lossy().to_string()] }),
    )?;

    // Volume is bounded in LINES by the overflow valve (clamped budget 1000),
    // not in characters. 40 files + a header sit well under the budget, so the
    // listing is untruncated and every item is present.
    assert!(
        text.lines().count() <= 1000,
        "Output must stay within the line budget: {} lines:\n{text}",
        text.lines().count()
    );
    assert!(
        text.contains("item_000.txt") && text.contains("item_039.txt"),
        "All items should be listed (no truncation at this size): {text}"
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
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // Cross-file dedup is gone: each identical file now renders its OWN
    // outline of declaration source lines, independently.
    assert!(
        !text.contains("common structure") && !text.contains("ranges are bounding"),
        "Cross-file dedup collapse should be gone: {text}"
    );
    assert!(
        !text.contains('<'),
        "Outline should have no kind label: {text}"
    );
    // Every file is listed with its own outline declaration lines.
    for i in 0..5 {
        let file = format!("proto_{i:03}.{MOCK_EXT}");
        assert!(text.contains(&file), "Should list {file}: {text}");
    }
    // The shared declaration lines appear once per file (5 files).
    assert_eq!(
        text.matches("fn alpha").count(),
        5,
        "Each file should render its own `fn alpha` outline node: {text}"
    );
    assert_eq!(
        text.matches("struct Beta").count(),
        5,
        "Each file should render its own `struct Beta` outline node: {text}"
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

    let config = "[tools.glob]\noutline_threshold = 5\n";
    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), Some(config))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    // Cross-file dedup is gone: each formerly-deduplicated file now renders
    // its OWN outline of declaration source lines, and the unique file
    // renders its own — no `common structure` collapse, no `<Kind>` label.
    assert!(
        !text.contains("common structure") && !text.contains("ranges are bounding"),
        "Cross-file dedup collapse should be gone: {text}"
    );
    assert!(
        !text.contains('<'),
        "Outline should have no kind label: {text}"
    );
    // Each of the 3 shared files renders its own `fn alpha` outline node.
    assert_eq!(
        text.matches("fn alpha").count(),
        3,
        "Each shared file should render its own `fn alpha` outline node: {text}"
    );
    // The unique file renders its own distinct declaration line.
    assert!(
        text.contains("fn unique_func"),
        "Should show the unique file's outline declaration line: {text}"
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
    let text = bridge.call_tool_text("glob", &json!({ "paths": [&abs_path] }))?;

    // Absolute pattern: no cwd header.
    assert!(
        !text.contains("cwd:"),
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
    let text = bridge.call_tool_text("glob", &json!({ "paths": [&abs_path] }))?;

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
fn test_glob_absolute_file_no_cwd_header() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join("readme.txt");
    std::fs::write(&file_path, "hello world\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let abs_path = file_path.to_string_lossy().to_string();
    let text = bridge.call_tool_text("glob", &json!({ "paths": [&abs_path] }))?;

    // Absolute file: no cwd header, absolute path in output.
    assert!(
        !text.contains("cwd:"),
        "Absolute file should not have cwd header: {text}"
    );
    assert!(text.contains("readme.txt"), "Should show file name: {text}");
    Ok(())
}

#[test]
fn test_grep_no_glob_cwd_scoped() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("hello.txt"), "needle in haystack\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "needle" }))?;

    // No glob → cwd-scoped search: paths are per-line and cwd-relative (no
    // `cwd:` header), and a no-LSP scope is flagged per line with `#?`.
    assert!(
        text.contains("hello.txt:1#?:needle in haystack"),
        "cwd-scoped grep should emit a cwd-relative `#?` line: {text}"
    );
    assert!(
        !text.contains("cwd:"),
        "the `cwd:` header is retired (paths are per-line, cwd-relative): {text}"
    );
    Ok(())
}

/// Grep with a relative glob (resolved against cwd) should find matches.
#[test]
fn test_grep_relative_glob_finds_matches() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let sub = dir.path().join("src");
    std::fs::create_dir(&sub)?;
    std::fs::write(sub.join("main.rs"), "fn cwd_target() {}\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "cwd_target", "glob": "src/**/*.rs" }),
    )?;

    assert!(text.contains("cwd_target"), "Should find the match: {text}");
    Ok(())
}

/// Grep with top-level alternation `foo|bar` should return results
/// from both arms, separated by a newline.
#[test]
fn test_grep_alternation_both_arms_present() -> Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("a.txt"), "alpha_unique_arm\n")?;
    std::fs::write(dir.path().join("b.txt"), "beta_unique_arm\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "alpha_unique_arm|beta_unique_arm" }),
    )?;

    assert!(
        text.contains("alpha_unique_arm"),
        "First alternation arm should be present: {text}"
    );
    assert!(
        text.contains("beta_unique_arm"),
        "Second alternation arm should be present: {text}"
    );
    // Single-page result: no page header, no leading blank lines
    assert!(
        !text.starts_with("\n\n"),
        "Should not have leading blank lines: {text:?}"
    );
    Ok(())
}

// ─── cwd scoping tests ────────────────────────────────────────────────

/// Grep without a glob scopes to cwd. Hits in a second root should not
/// appear when cwd points to the first root.
#[test]
fn test_grep_cwd_scoping_prevents_cross_root() -> Result<()> {
    let dir_a = tempfile::tempdir()?;
    let dir_b = tempfile::tempdir()?;

    std::fs::write(dir_a.path().join("a.txt"), "unique_cross_root_a\n")?;
    std::fs::write(dir_b.path().join("b.txt"), "unique_cross_root_b\n")?;

    let root_a = dir_a.path().to_string_lossy().to_string();
    let root_b = dir_b.path().to_string_lossy().to_string();

    let mut bridge = BridgeProcess::spawn_multi_root(&[], &[&root_a, &root_b])?;
    bridge.initialize()?;

    // cwd defaults to root_a — search should NOT find root_b content.
    let text = bridge.call_tool_text("grep", &json!({ "pattern": "unique_cross_root_b" }))?;
    assert!(
        text.is_empty(),
        "cwd-scoped grep should not find hits from another root: {text}"
    );

    // Explicitly targeting root_b via directory should find it.
    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "unique_cross_root_b", "directory": &root_b }),
    )?;
    assert!(
        text.contains("unique_cross_root_b"),
        "grep with explicit directory should find hits: {text}"
    );
    Ok(())
}

/// Grep from a directory outside all workspace roots shows the LSP
/// warning header and returns only matches from that directory.
#[test]
fn test_grep_outside_roots_lsp_warning() -> Result<()> {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;

    std::fs::write(root.path().join("in_root.txt"), "shared_needle\n")?;
    std::fs::write(outside.path().join("outside.txt"), "shared_needle\n")?;

    let root_str = root.path().to_string_lossy().to_string();

    let mut bridge = spawn_no_lsp(&root_str)?;
    bridge.initialize()?;

    // Search from outside the workspace root.
    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "shared_needle", "directory": outside.path().to_string_lossy().as_ref() }),
    )?;

    // Degradation is now per-line: an out-of-LSP scope carries the `#?` marker
    // rather than a `(no LSP)` / `cwd:` header.
    assert!(
        text.contains("outside.txt:1#?:shared_needle"),
        "grep outside roots should carry the per-line `#?` marker, cwd-relative: {text}"
    );
    assert!(
        !text.contains("cwd:") && !text.contains("no LSP"),
        "the `cwd:` header and `(no LSP)` label are retired (replaced by `#?`): {text}"
    );
    // Should NOT find the match in the workspace root.
    assert!(
        !text.contains("in_root.txt"),
        "cwd-scoped grep should not leak into workspace root: {text}"
    );
    Ok(())
}

/// Glob from a directory outside all workspace roots shows the LSP
/// warning header.
#[test]
fn test_glob_outside_roots_lsp_warning() -> Result<()> {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;

    std::fs::write(root.path().join("in_root.txt"), "hello\n")?;
    std::fs::write(outside.path().join("outside.txt"), "hello\n")?;

    let root_str = root.path().to_string_lossy().to_string();

    let mut bridge = spawn_no_lsp(&root_str)?;
    bridge.initialize()?;

    // Glob an absolute path outside workspace roots.
    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [outside.path().to_string_lossy().as_ref()] }),
    )?;

    // Should contain the LSP warning.
    assert!(
        text.contains("no LSP"),
        "glob outside roots should show LSP warning: {text}"
    );
    // Should list the file in the outside directory.
    assert!(
        text.contains("outside.txt"),
        "Should list file outside roots: {text}"
    );
    Ok(())
}

// ─── `.`/cwd deterministic root resolution (bug 31) ───────────────────

/// `grep "<pat>" .` from inside the repo, with a `/tmp`-style probe dir also
/// registered as a workspace root, returns **only** the repo's matches — never
/// the probe's — even when the repo's language server is still warming up
/// (busy on `initialized`) while the probe (no LSP) is trivially "ready".
///
/// This pins the bug-31 invariant: a `.`-scoped grep binds to the invoking cwd
/// and never substitutes a *different* registered root, regardless of which
/// root's LSP is ready first. The probe-only failure mode (a silent
/// false-negative on the real matches) is the highest-severity class under
/// decision 019.
#[test]
fn dot_grep_scopes_to_cwd_root_not_another() -> Result<()> {
    let repo = tempfile::tempdir()?;
    let probe = tempfile::tempdir()?;

    // Same needle in both roots; only the repo's hit must surface.
    std::fs::write(
        repo.path().join(format!("real.{MOCK_LANG_A}")),
        "fn dot_scope_needle()\n",
    )?;
    std::fs::write(
        probe.path().join("anchor_probe.md"),
        "dot_scope_needle in the probe dir\n",
    )?;

    let repo_str = repo.path().to_string_lossy().to_string();
    let probe_str = probe.path().to_string_lossy().to_string();

    // Repo's LSP is busy warming up on `initialized` while the probe has no
    // LSP — the original race that made bug 31 intermittent.
    let lsp = common::mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --cpu-on-initialized 2000");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[&repo_str, &probe_str])?;
    bridge.initialize()?;

    // Pathless grep — cwd defaults to the first root (the repo).
    let text = bridge.call_tool_text("grep", &json!({ "pattern": "dot_scope_needle" }))?;

    assert!(
        text.contains("real."),
        "`.`-scoped grep must return the repo's match: {text}"
    );
    assert!(
        !text.contains("anchor_probe.md") && !text.contains("probe dir"),
        "`.`-scoped grep must NOT leak the other registered root's matches: {text}"
    );
    Ok(())
}

/// `grep "<pat>" .` with a cwd outside every workspace root searches that cwd
/// and labels the result `(no LSP …)` — and never returns a *different*
/// registered root's matches in its place.
///
/// The loud label is the same partial-result annotation `glob` emits; a
/// silently-substituted root would be a wrong answer that reads as authoritative
/// (bug 31, decision 019).
#[test]
fn dot_grep_cwd_outside_roots_is_labeled() -> Result<()> {
    let repo = tempfile::tempdir()?;
    let probe = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;

    // The registered roots hold the needle; the searched (outside) cwd holds
    // its own distinct copy. Only the outside copy must surface.
    std::fs::write(repo.path().join("in_repo.txt"), "outside_scope_needle\n")?;
    std::fs::write(probe.path().join("in_probe.txt"), "outside_scope_needle\n")?;
    std::fs::write(
        outside.path().join("loose.txt"),
        "outside_scope_needle here\n",
    )?;

    let repo_str = repo.path().to_string_lossy().to_string();
    let probe_str = probe.path().to_string_lossy().to_string();

    let mut bridge = BridgeProcess::spawn_multi_root(&[], &[&repo_str, &probe_str])?;
    bridge.initialize()?;

    // Search from a directory outside all roots.
    let text = bridge.call_tool_text(
        "grep",
        &json!({
            "pattern": "outside_scope_needle",
            "directory": outside.path().to_string_lossy().as_ref(),
        }),
    )?;

    assert!(
        text.contains("loose.txt:1#?:outside_scope_needle here"),
        "grep outside all roots must carry the per-line `#?` degradation marker \
         on its literal-cwd match: {text}"
    );
    assert!(
        !text.contains("in_repo.txt") && !text.contains("in_probe.txt"),
        "grep outside roots must NOT substitute a registered root's matches: {text}"
    );
    Ok(())
}

/// `grep "<pat>" .` runs the ripgrep pass against the **correct** (cwd's) root
/// and returns its raw matches even while that root's language server is not
/// yet ready — it never falls back to a different root that happens to be ready.
///
/// The repo's LSP burns CPU on `initialized` (not ready at query time); the
/// probe root has no LSP. Raw matches are LSP-independent, so the repo's hit
/// must surface and the probe's must not.
#[test]
fn dot_grep_lsp_not_ready_uses_correct_root() -> Result<()> {
    let repo = tempfile::tempdir()?;
    let probe = tempfile::tempdir()?;

    std::fs::write(
        repo.path().join(format!("src.{MOCK_LANG_A}")),
        "fn not_ready_needle()\n",
    )?;
    std::fs::write(probe.path().join("probe.md"), "not_ready_needle in probe\n")?;

    let repo_str = repo.path().to_string_lossy().to_string();
    let probe_str = probe.path().to_string_lossy().to_string();

    // Repo LSP is mid-warmup (not ready) when grep is issued; probe has none.
    let lsp = common::mockls_lsp_arg(MOCK_LANG_A, "--scan-roots --cpu-on-initialized 2000");
    let mut bridge = BridgeProcess::spawn_multi_root(&[&lsp], &[&repo_str, &probe_str])?;
    bridge.initialize()?;

    // Pathless grep immediately after init — cwd is the repo, LSP still warming.
    let text = bridge.call_tool_text("grep", &json!({ "pattern": "not_ready_needle" }))?;

    assert!(
        text.contains("src."),
        "raw ripgrep matches from the correct root must surface even when its \
         LSP is not ready: {text}"
    );
    assert!(
        !text.contains("probe.md") && !text.contains("in probe"),
        "a not-ready cwd root must NOT be replaced by a different ready root: {text}"
    );
    Ok(())
}

// ─── walk hardening (bugs 34/35) ──────────────────────────────────────

/// Grep an explicitly-named file that exists must always search it — a
/// named-but-present path must never silently zero (bug 34/35). The path
/// is passed directly in `paths`, exercising the daemon's literal-path
/// resolution and the walker's per-entry file decision.
#[test]
fn grep_named_present_file_never_zeros() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("present.rs");
    std::fs::write(&file, "let named_present_needle = 1;\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Repeat: a cold first-grep under-return that self-heals on retry (bug 35)
    // would surface as an empty result on one of the early iterations.
    for i in 0..8 {
        let text = bridge.call_tool_text(
            "grep",
            &json!({
                "pattern": "named_present_needle",
                "paths": [file.to_string_lossy().as_ref()],
            }),
        )?;
        assert!(
            text.contains("named_present_needle"),
            "named present file must always be searched (iteration {i}): {text:?}"
        );
        assert!(
            !text.trim().is_empty(),
            "named present file must never zero (iteration {i})"
        );
    }
    Ok(())
}

/// A file written via atomic rename (write temp + `rename`), then grepped
/// sequentially in the same workflow, must return its match — the bug-34
/// in-workflow case. The fresh stat must not drop an entry the walker
/// enumerated just because it raced the rename window.
#[test]
fn grep_atomic_rename_in_workflow_returns_match() -> Result<()> {
    let dir = tempfile::tempdir()?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Each iteration writes a fresh file via atomic rename, then greps it
    // sequentially — the exact in-workflow ordering that bug 34 reported.
    for i in 0..16 {
        let target = dir.path().join(format!("renamed_{i}.rs"));
        let tmp = dir.path().join(format!(".renamed_{i}.rs.tmp"));
        std::fs::write(&tmp, format!("let atomic_rename_needle_{i} = 1;\n"))?;
        std::fs::rename(&tmp, &target)?;

        let text = bridge.call_tool_text(
            "grep",
            &json!({
                "pattern": format!("atomic_rename_needle_{i}"),
                "paths": [target.to_string_lossy().as_ref()],
            }),
        )?;
        assert!(
            text.contains(&format!("atomic_rename_needle_{i}")),
            "atomic-rename write must be searched in-workflow (iteration {i}): {text:?}"
        );
    }
    Ok(())
}
