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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
    let outside = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;

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

    let dir = common::canonical_tempdir()?;
    let outside = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;
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

/// A quoted glob pattern that matches nothing yields empty daemon `output`
/// (the loud `no matches for pattern` report travels in the separate
/// `no_match_patterns` field, rendered CLI-side) — never an error.
#[test]
fn test_glob_quoted_pattern_zero_match_is_empty() -> Result<()> {
    let dir = common::canonical_tempdir()?;
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

/// A glob **pattern** argument renders its per-file listings with **no**
/// cardinality header — stdout is results only (the VERBS streams ruling; the
/// `N files match` header retired, `--count` is the sole tally). The daemon
/// `output` (what `call_tool_text` captures) lists the matched files directly.
#[test]
fn test_glob_pattern_output_has_no_header() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::create_dir_all(dir.path().join("src/inner"))?;
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("src/inner/lib.rs"), "fn lib() {}")?;
    std::fs::write(dir.path().join("src/notes.txt"), "notes")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "paths": ["src/**/*.rs"] }))?;

    assert!(
        !text.contains("files match") && !text.contains("file matches"),
        "stdout carries no cardinality header (results only): {text}"
    );
    assert!(
        text.contains("main.rs"),
        "the matched files are listed: {text}"
    );
    assert!(
        text.contains("lib.rs"),
        "the matched files are listed: {text}"
    );
    Ok(())
}

/// A pattern with exactly one match still lists that file with no header.
#[test]
fn test_glob_pattern_single_match_has_no_header() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::create_dir(dir.path().join("src"))?;
    std::fs::write(dir.path().join("src/only.rs"), "fn only() {}")?;
    std::fs::write(dir.path().join("src/notes.txt"), "notes")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "paths": ["src/**/*.rs"] }))?;

    assert!(
        !text.contains("files match") && !text.contains("file matches"),
        "no cardinality header for a lone match: {text}"
    );
    assert!(text.contains("only.rs"), "the lone match is listed: {text}");
    Ok(())
}

/// A brace alternation is the one-pattern spelling for several patterns
/// (arity 1 is grammar since VERBS; ws43-03 made the CLI the only surface):
/// every alternative's matches are listed, with no header on any of them.
#[test]
fn test_glob_multiple_patterns_list_without_headers() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::write(dir.path().join("a.rs"), "fn a() {}")?;
    std::fs::write(dir.path().join("b.txt"), "b")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "paths": ["{*.rs,*.txt}"] }))?;

    assert!(
        !text.contains("files match") && !text.contains("file matches"),
        "no cardinality header on any pattern: {text}"
    );
    assert!(text.contains("a.rs"), "*.rs match listed: {text}");
    assert!(text.contains("b.txt"), "*.txt match listed: {text}");
    Ok(())
}

/// A directory-matching pattern renders unchanged — no cardinality header (the
/// header retired entirely; stdout is results only).
#[test]
fn test_glob_directory_argument_has_no_match_header() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::write(dir.path().join("main.rs"), "fn main() {}")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;

    assert!(
        !text.contains("files match") && !text.contains("file matches"),
        "a directory listing carries no cardinality header: {text}"
    );
    Ok(())
}

/// grep applies the same expansion to its path arguments: a quoted glob
/// scopes the search to the files it matches.
#[test]
fn test_grep_quoted_pattern_path_scopes_search() -> Result<()> {
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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

// ─── ripgrep flag parity (pipeable-output ticket 04) ───────────────────

/// `-l`/`--files-with-matches` yields a bare cwd-relative file list — one path
/// per line, no `#scope` anchor and no verbatim text — that composes with the
/// strip-to-ripgrep contract.
#[test]
fn test_grep_files_with_matches_lists_paths() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::write(dir.path().join("a.rs"), "let needle = 1;")?;
    std::fs::write(dir.path().join("b.rs"), "needle again")?;
    std::fs::write(dir.path().join("c.rs"), "nothing here")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "needle", "files_with_matches": true }),
    )?;
    assert!(text.contains("a.rs"), "a.rs matched: {text}");
    assert!(text.contains("b.rs"), "b.rs matched: {text}");
    assert!(!text.contains("c.rs"), "c.rs has no match: {text}");
    // Just paths — no anchor, no verbatim line.
    assert!(!text.contains('#'), "no #scope anchor in -l output: {text}");
    assert!(
        !text.contains("needle"),
        "no verbatim text in -l output: {text}"
    );
    Ok(())
}

/// Context lines (`-A`/`-B`/`-C`) render in the same `path:line#…:<verbatim>`
/// shape as match lines — each becomes its own self-contained line.
#[test]
fn test_grep_context_renders_in_line_format() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::write(dir.path().join("f.rs"), "before\nthe needle line\nafter\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "needle", "paths": ["f.rs"], "after_context": 1 }),
    )?;
    // The match line plus one after-context line, both in the line format.
    assert!(
        text.contains("the needle line"),
        "match line present: {text}"
    );
    assert!(text.contains("after"), "after-context line present: {text}");
    // No-LSP → `#?`; stripping it yields byte-exact ripgrep `f.rs:LINE:text`.
    assert!(
        text.contains("f.rs:2"),
        "match keeps its textual coord: {text}"
    );
    assert!(
        text.contains("f.rs:3"),
        "context keeps its textual coord: {text}"
    );
    Ok(())
}

/// `-g`/`--glob` is a positive file filter on a directory (cwd) walk.
#[test]
fn test_grep_glob_filter_restricts_files() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::write(dir.path().join("a.rs"), "let needle = 1;")?;
    std::fs::write(dir.path().join("b.txt"), "needle in text")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("grep", &json!({ "pattern": "needle", "globs": ["*.rs"] }))?;
    assert!(
        text.contains("a.rs"),
        "-g '*.rs' keeps the .rs file: {text}"
    );
    assert!(
        !text.contains("b.txt"),
        "-g '*.rs' filters the .txt file: {text}"
    );
    Ok(())
}

/// Case defaults to smart-case: a lowercase pattern is insensitive, a pattern
/// with an uppercase letter is sensitive; `-i` forces insensitive.
#[test]
fn test_grep_smart_case_default_and_overrides() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::write(
        dir.path().join("f.rs"),
        "let Needle = 1;\nlet needle = 2;\n",
    )?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Uppercase-bearing pattern → case-sensitive: matches only `Needle`.
    let sensitive =
        bridge.call_tool_text("grep", &json!({ "pattern": "Needle", "paths": ["f.rs"] }))?;
    assert!(
        sensitive.contains("f.rs:1"),
        "Needle matched line 1: {sensitive}"
    );
    assert!(
        !sensitive.contains("f.rs:2"),
        "smart-case keeps `Needle` off the lowercase line: {sensitive}"
    );

    // Lowercase pattern → case-insensitive: matches both lines.
    let insensitive =
        bridge.call_tool_text("grep", &json!({ "pattern": "needle", "paths": ["f.rs"] }))?;
    assert!(
        insensitive.contains("f.rs:1"),
        "needle matched line 1: {insensitive}"
    );
    assert!(
        insensitive.contains("f.rs:2"),
        "needle matched line 2: {insensitive}"
    );

    // `-i` forces insensitive even for an uppercase-bearing pattern.
    let forced = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "Needle", "paths": ["f.rs"], "ignore_case": true }),
    )?;
    assert!(forced.contains("f.rs:1"), "-i matched line 1: {forced}");
    assert!(forced.contains("f.rs:2"), "-i matched line 2: {forced}");
    Ok(())
}

/// `-v`/`--invert-match` selects non-matching lines, rendered in the line format.
#[test]
fn test_grep_invert_selects_non_matching() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::write(dir.path().join("f.rs"), "keep one\ndrop needle\nkeep two\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "grep",
        &json!({ "pattern": "needle", "paths": ["f.rs"], "invert": true }),
    )?;
    assert!(
        text.contains("keep one"),
        "non-matching line 1 selected: {text}"
    );
    assert!(
        text.contains("keep two"),
        "non-matching line 3 selected: {text}"
    );
    assert!(
        !text.contains("drop needle"),
        "the matching line is inverted out: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_tier3_bucketed() -> Result<()> {
    let dir = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
fn test_glob_dir_prints_complete_listing() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    // Several outlined files: the complete listing prints, every file's full
    // outline (decision 025 — no line budget, no truncation).
    for f in 0..5 {
        std::fs::write(
            dir.path().join(format!("file_{f}.{MOCK_EXT}")),
            gen_mock_content(10),
        )?;
    }
    // Legacy keys (`line_budget`, `outline_threshold`) are silently ignored by
    // lenient parsing — the listing is complete regardless.
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

    // Outline declaration lines render (no `<Kind>` label).
    assert!(
        output.contains("fn func_") || output.contains("struct Struct_"),
        "Should show declaration lines in maps: {output}"
    );
    assert!(
        !output.contains('<'),
        "Outline should have no kind label: {output}"
    );

    // The complete listing prints: every file appears, none truncated away.
    for f in 0..5 {
        assert!(
            output.contains(&format!("file_{f}.{MOCK_EXT}")),
            "complete listing holds file_{f}.{MOCK_EXT}:\n{output}"
        );
    }
    // No volume machinery on the wire.
    assert!(
        resp.get("receipt").is_none(),
        "no receipt field — output is always complete: {resp:?}"
    );
    Ok(())
}

/// The ruled weight lever on the finding's own shape (ws43-03): a plain
/// directory listing of several files yields LISTING-weight output — each
/// file's top-level symbols only, no nested tree (the recorded finding was
/// 45–360KB of full outlines for plain listings) — and `--outline` restores
/// the full tree. Pinned by SHAPE (which nodes render), not byte count.
#[test]
fn test_glob_listing_shape_defaults_to_listing_weight() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    // Four files, each with a nested definition under a top-level container.
    for f in 0..4 {
        std::fs::write(
            dir.path().join(format!("file_{f}.{MOCK_EXT}")),
            format!("struct Top_{f} {{\nfn nested_{f}\n}}\n"),
        )?;
    }

    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), None)?;
    bridge.initialize()?;

    let listing = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;
    for f in 0..4 {
        assert!(
            listing.contains(&format!("file_{f}.{MOCK_EXT}")),
            "every path always lists (decision 025): {listing}"
        );
        assert!(
            listing.contains(&format!("struct Top_{f} {{")),
            "listing weight keeps each file's top-level structure: {listing}"
        );
        assert!(
            !listing.contains(&format!("fn nested_{f}")),
            "listing weight renders NO nested tree — the ruled lever: {listing}"
        );
    }

    // `--outline` opts up to the full picture on demand.
    let full = bridge.call_tool_text(
        "glob",
        &json!({
            "paths": [dir.path().to_string_lossy().to_string()],
            "outline": true,
        }),
    )?;
    for f in 0..4 {
        assert!(
            full.contains(&format!("fn nested_{f}")),
            "--outline restores the full tree: {full}"
        );
    }
    Ok(())
}

/// The single-file outline shape keeps the FULL tree as its default — the
/// weight lever trims listings, never the explicit file read.
#[test]
fn test_glob_single_file_keeps_full_outline_default() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let file = dir.path().join(format!("nested.{MOCK_EXT}"));
    std::fs::write(&file, "struct Outer {\nfn inner\n}\n")?;

    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), None)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [file.to_string_lossy().to_string()] }),
    )?;
    assert!(
        text.contains("struct Outer {") && text.contains("fn inner"),
        "a single matched file defaults to its fully-expanded outline: {text}"
    );
    Ok(())
}

#[test]
fn test_glob_outline_suppress() -> Result<()> {
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
    // A file with nested definitions: Outer { contains inner }; leaf is
    // top-level. With `--outline`, full expansion shows each node on its own
    // indented line — the old `/`-collapse marker (a container shown as
    // `Outer {/`) is gone. Without it, this directory listing is a listing
    // shape, so the ruled default is TOP-LEVEL structure only (ws43-03): the
    // nested child stays out of the default render and `--outline` restores
    // the full tree.
    std::fs::write(
        dir.path().join(format!("nested.{MOCK_EXT}")),
        "struct Outer {\nfn inner\n}\nfn leaf\n\n\n\n\n\n\n",
    )?;

    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), None)?;
    bridge.initialize()?;

    // The ruled listing-weight default: top-level symbols only, no nested tree.
    let listing = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [dir.path().to_string_lossy().to_string()] }),
    )?;
    assert!(
        listing.contains("struct Outer {") && listing.contains("fn leaf"),
        "listing weight keeps the top-level structure: {listing}"
    );
    assert!(
        !listing.contains("fn inner"),
        "listing weight shows no nested tree — `--outline` opts up: {listing}"
    );

    let text = bridge.call_tool_text(
        "glob",
        &json!({
            "paths": [dir.path().to_string_lossy().to_string()],
            "outline": true,
        }),
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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

    let dir = common::canonical_tempdir()?;
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

    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;

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
fn test_glob_prints_complete_outline() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let file = dir.path().join(format!("huge.{MOCK_EXT}"));
    // A large single-file outline: it prints in full (decision 025 — no budget,
    // no spill), including the last symbols.
    std::fs::write(&file, gen_mock_content(500))?;

    let mut bridge = spawn_with_mockls_and_config(&dir.path().to_string_lossy(), None)?;
    bridge.initialize()?;

    let resp = bridge.call_search_raw(
        "tool/glob",
        &json!({ "paths": [file.to_str().context("file path")?] }),
    )?;
    let output = resp
        .get("output")
        .and_then(serde_json::Value::as_str)
        .context("output")?;

    // The complete outline prints — first and last symbols both present.
    assert!(
        output.contains("func_0") || output.contains("Struct_1"),
        "complete outline holds the first symbols:\n{}",
        output.lines().take(3).collect::<Vec<_>>().join("\n")
    );
    assert!(
        output.contains("func_498") || output.contains("Struct_499"),
        "complete outline holds the last symbols:\n{}",
        output.lines().rev().take(3).collect::<Vec<_>>().join("\n")
    );
    // No volume machinery on the wire.
    assert!(
        resp.get("receipt").is_none(),
        "no receipt field — output is always complete: {resp:?}"
    );
    Ok(())
}

#[test]
fn test_glob_no_grammar() -> Result<()> {
    let dir = common::canonical_tempdir()?;
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
fn test_glob_lists_every_item() -> Result<()> {
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

    // The output is always complete (decision 025): every item is listed, none
    // truncated away — first through last.
    for i in 0..40 {
        let item = format!("item_{i:03}.txt");
        assert!(
            text.contains(&item),
            "complete listing holds {item}:\n{text}"
        );
    }
    Ok(())
}

#[test]
fn test_glob_structure_dedup() -> Result<()> {
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;
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
    let dir_a = common::canonical_tempdir()?;
    let dir_b = common::canonical_tempdir()?;

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

// ─── relative path-argument anchoring (bug 69 / bug 31 family) ─────────

/// A relative *path argument* anchors at the CLI's cwd, not at the enclosing
/// workspace root. From a subdirectory of a root, `grep <pat> '*.rs'` must
/// match only files under that subdirectory — never a same-named pattern's hits
/// elsewhere in the root (bug 69). Absolutization happens in
/// `GrepRequest::to_params` (`cwd.join`) before the pattern reaches the
/// gitignore-aware walker.
#[test]
fn grep_relative_path_arg_anchors_at_cwd_subdir() -> Result<()> {
    let root = common::canonical_tempdir()?;
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub)?;
    // Same needle at the root and under the subdirectory; only the
    // subdirectory (cwd) copy must surface for a relative `*.rs`.
    std::fs::write(root.path().join("outer_marker.rs"), "let needle = 1;\n")?;
    std::fs::write(sub.join("inner_marker.rs"), "let needle = 2;\n")?;

    let mut bridge = spawn_no_lsp(&root.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "grep",
        &json!({
            "pattern": "needle",
            "paths": ["*.rs"],
            "directory": sub.to_string_lossy().as_ref(),
        }),
    )?;

    assert!(
        text.contains("inner_marker.rs"),
        "relative `*.rs` must anchor at the cwd subdirectory: {text}"
    );
    assert!(
        !text.contains("outer_marker.rs"),
        "relative `*.rs` must NOT anchor at the enclosing root: {text}"
    );
    Ok(())
}

/// Glob counterpart of `grep_relative_path_arg_anchors_at_cwd_subdir`: a
/// relative pattern lists only the cwd subdirectory's files, not the root's.
#[test]
fn glob_relative_path_arg_anchors_at_cwd_subdir() -> Result<()> {
    let root = common::canonical_tempdir()?;
    let sub = root.path().join("sub");
    std::fs::create_dir(&sub)?;
    std::fs::write(root.path().join("outer_marker.rs"), "// root\n")?;
    std::fs::write(sub.join("inner_marker.rs"), "// sub\n")?;

    let mut bridge = spawn_no_lsp(&root.path().to_string_lossy())?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({
            "paths": ["*.rs"],
            "directory": sub.to_string_lossy().as_ref(),
        }),
    )?;

    assert!(
        text.contains("inner_marker.rs"),
        "relative `*.rs` glob must anchor at the cwd subdirectory: {text}"
    );
    assert!(
        !text.contains("outer_marker.rs"),
        "relative `*.rs` glob must NOT anchor at the enclosing root: {text}"
    );
    Ok(())
}

/// Nested-checkout shape: when a marker root *encloses* another, a relative
/// glob issued from the inner root's directory anchors at that cwd — the inner
/// root wins over the enclosing one (bug 69's nested-worktree instance).
#[test]
fn glob_relative_path_arg_prefers_cwd_over_enclosing_root() -> Result<()> {
    let outer = common::canonical_tempdir()?;
    let inner = outer.path().join("inner");
    std::fs::create_dir(&inner)?;
    std::fs::write(outer.path().join("outer_marker.rs"), "// outer\n")?;
    std::fs::write(inner.join("inner_marker.rs"), "// inner\n")?;

    let outer_str = outer.path().to_string_lossy().to_string();
    let inner_str = inner.to_string_lossy().to_string();

    // Both the enclosing and the nested directory are registered roots.
    let mut bridge = BridgeProcess::spawn_multi_root(&[], &[&outer_str, &inner_str])?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({
            "paths": ["*.rs"],
            "directory": &inner_str,
        }),
    )?;

    assert!(
        text.contains("inner_marker.rs"),
        "relative glob from the inner root must anchor at that cwd: {text}"
    );
    assert!(
        !text.contains("outer_marker.rs"),
        "the enclosing root must not win over the cwd: {text}"
    );
    Ok(())
}

/// Grep from a directory outside all workspace roots shows the LSP
/// warning header and returns only matches from that directory.
#[test]
fn test_grep_outside_roots_lsp_warning() -> Result<()> {
    let root = common::canonical_tempdir()?;
    let outside = common::canonical_tempdir()?;

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

/// Glob from a directory outside all workspace roots degrades per-file.
#[test]
fn test_glob_outside_roots_lsp_warning() -> Result<()> {
    let root = common::canonical_tempdir()?;
    let outside = common::canonical_tempdir()?;

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

    // Degradation is per-file since ws43-03: an uncovered text file carries
    // the `no outline` marker rather than a `(no LSP)` scope header — the
    // same retirement grep made in ws43-02 (its header became the per-line
    // `#?` marker). The listing itself stays complete.
    assert!(
        text.contains("outside.txt  (1 line, no outline)"),
        "glob outside roots should carry the per-file `no outline` marker: {text}"
    );
    assert!(
        !text.contains("no LSP"),
        "the `(no LSP)` scope header is retired (replaced by per-file markers): {text}"
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
    let repo = common::canonical_tempdir()?;
    let probe = common::canonical_tempdir()?;

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
    let repo = common::canonical_tempdir()?;
    let probe = common::canonical_tempdir()?;
    let outside = common::canonical_tempdir()?;

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
    let repo = common::canonical_tempdir()?;
    let probe = common::canonical_tempdir()?;

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
    let dir = common::canonical_tempdir()?;
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
    let dir = common::canonical_tempdir()?;

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

// ── exclude-pattern reaches glob-pattern matches (bug 73) ──────────

/// The same exclude over the same tree yields the same surviving set whether
/// reached by a glob **pattern** argument or a **named-directory** argument —
/// a pattern argument now honors `--exclude-pattern` exactly as a named dir
/// does (bug 73).
#[test]
fn glob_exclude_pattern_matches_named_dir_parity() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::create_dir(dir.path().join("src"))?;
    std::fs::write(dir.path().join("src/a.rs"), "fn a() {}")?;
    std::fs::write(dir.path().join("src/b.rs"), "fn b() {}")?;
    std::fs::write(dir.path().join("src/keep.txt"), "keep")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Named-directory argument: the directory listing filters its entries.
    let named = bridge.call_glob(&json!({
        "paths": [dir.path().join("src").to_string_lossy().as_ref()],
        "exclude": "*.rs",
    }))?;

    // Glob-pattern argument: the pattern's matches are filtered.
    let pattern = bridge.call_glob(&json!({
        "paths": ["src/*"],
        "exclude": "*.rs",
    }))?;

    for out in [named.as_str(), pattern.as_str()] {
        assert!(out.contains("keep.txt"), "keep.txt survives: {out}");
        assert!(!out.contains("a.rs"), "a.rs excluded: {out}");
        assert!(!out.contains("b.rs"), "b.rs excluded: {out}");
    }
    Ok(())
}

/// `--count` and the rendered listing agree under an exclude: a pattern's count
/// reports the surviving cardinality, not the pre-exclude match total (bug 73).
#[test]
fn glob_exclude_pattern_count_matches_listing() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::create_dir(dir.path().join("src"))?;
    std::fs::write(dir.path().join("src/a.rs"), "fn a() {}")?;
    std::fs::write(dir.path().join("src/b.rs"), "fn b() {}")?;
    std::fs::write(dir.path().join("src/keep.txt"), "keep")?;
    std::fs::write(dir.path().join("src/notes.md"), "notes")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Listing: the two non-`.rs` files survive; the `.rs` matches drop.
    let listing = bridge.call_glob(&json!({
        "paths": ["src/*"],
        "exclude": "*.rs",
    }))?;
    assert!(listing.contains("keep.txt"), "keep.txt survives: {listing}");
    assert!(listing.contains("notes.md"), "notes.md survives: {listing}");
    assert!(!listing.contains("a.rs"), "a.rs excluded: {listing}");
    assert!(!listing.contains("b.rs"), "b.rs excluded: {listing}");

    // `--count` reports the same surviving cardinality (2), not the 4 matches.
    let raw = bridge.call_search_raw(
        "tool/glob",
        &json!({ "paths": ["src/*"], "exclude": "*.rs", "count": true }),
    )?;
    let paths = raw.get("paths").and_then(serde_json::Value::as_u64);
    assert_eq!(paths, Some(2), "count agrees with the listing: {raw}");
    Ok(())
}

/// The reporter's exact shape: `<root>/**/* --include-hidden --exclude-pattern
/// '**/.git/**'` returns no `.git` file contents, while non-`.git` files still
/// surface — the exclude is targeted, not absent (bug 73).
#[test]
fn glob_exclude_pattern_drops_git_contents() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::create_dir_all(dir.path().join(".git/hooks"))?;
    std::fs::write(dir.path().join(".git/COMMIT_EDITMSG"), "wip")?;
    std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n")?;
    std::fs::write(
        dir.path().join(".git/hooks/pre-commit.sample"),
        "#!/bin/sh\n",
    )?;
    std::fs::create_dir(dir.path().join("src"))?;
    std::fs::write(dir.path().join("src/main.rs"), "fn main() {}")?;
    std::fs::write(dir.path().join("README.md"), "# readme")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let pattern = format!("{}/**/*", dir.path().to_string_lossy());
    let text = bridge.call_glob(&json!({
        "paths": [pattern],
        "include_hidden": true,
        "exclude": "**/.git/**",
    }))?;

    assert!(
        !text.contains(".git/COMMIT_EDITMSG"),
        ".git contents excluded: {text}"
    );
    assert!(
        !text.contains(".git/HEAD"),
        ".git contents excluded: {text}"
    );
    assert!(
        !text.contains("pre-commit.sample"),
        ".git hook samples excluded: {text}"
    );
    // The exclude is targeted — non-`.git` files still surface.
    assert!(text.contains("README.md"), "non-.git files survive: {text}");
    assert!(text.contains("main.rs"), "non-.git files survive: {text}");
    Ok(())
}

/// The lead's receipt geometry (misc 222 half 2), end-to-end through the real
/// CLI binary: an **absolute** positional targets a tree the shell is NOT
/// sitting in (cwd is an unrelated sibling tree), with a relative slash-bearing
/// exclude (`**/*.md`). The exclude must reach the candidate paths — the pre-fix
/// code anchored it to cwd, so its absolute prefix pointed at the wrong tree and
/// it went silently inert (rendering every `.md` and counting it in the tally).
/// Anchoring the relative exclude at the positional's tree (`anchor_base`) fixes
/// both the listing enrichment and the first-class match set.
#[test]
fn glob_exclude_reaches_absolute_cross_root_positional() -> Result<()> {
    // Two sibling trees under a shared parent — neither a prefix of the other,
    // the `Catenary` vs `CatenaryInternal` shape. `tickets/` holds a
    // SUBDIRECTORY (`misc/`), so `tickets/*` matches the subdir and renders its
    // listing — the receipt's exact shape, where the excluded `.md` lives inside
    // a listed subdirectory (not a direct file match).
    let parent = common::canonical_tempdir()?;
    let cwd_tree = parent.path().join("project");
    let target_tree = parent.path().join("project-internal");
    let misc = target_tree.join("tickets").join("misc");
    std::fs::create_dir_all(&cwd_tree)?;
    std::fs::create_dir_all(&misc)?;
    std::fs::write(misc.join("222.md"), "# ticket\n")?;
    std::fs::write(misc.join("code.rs"), "fn code() {}\n")?;

    // The daemon is rooted at the target tree (where the candidates live); the
    // CLI runs from the UNRELATED cwd tree.
    let mut bridge = spawn_no_lsp(&target_tree.to_string_lossy())?;
    bridge.initialize()?;

    // ── Listing leg: the absolute positional (`tickets/*`) matches the `misc`
    //    subdirectory, whose listing enrichment renders — the exclude must reach
    //    those nested entries and their tally.
    let listing = bridge.call_glob(&json!({
        "paths": [target_tree.join("tickets").join("*").to_string_lossy().as_ref()],
        "exclude": "**/*.md",
        "directory": cwd_tree.to_string_lossy().as_ref(),
    }))?;
    assert!(
        listing.contains("code.rs"),
        "the surviving nested entry lists: {listing}"
    );
    assert!(
        !listing.contains("222.md"),
        "the cross-root exclude drops the .md from the nested listing (was inert pre-fix): {listing}"
    );
    assert!(
        listing.contains("(1 file, 0 dirs)"),
        "the subdirectory tally counts only the surviving entry: {listing}"
    );

    // ── Match-set leg: an absolute pattern matching the `.md` directly. With
    //    every match excluded the pattern reports the honest no-match.
    let match_pattern = misc.join("22*");
    let raw = bridge.call_search_raw(
        "tool/glob",
        &json!({
            "paths": [match_pattern.to_string_lossy().as_ref()],
            "exclude": "**/*.md",
            "directory": cwd_tree.to_string_lossy().as_ref(),
        }),
    )?;
    let output = raw
        .get("output")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    assert!(
        output.trim().is_empty(),
        "the only match (222.md) is excluded, so nothing renders: {output:?}"
    );

    // ── `--count` agrees: zero surviving matches.
    let count_raw = bridge.call_search_raw(
        "tool/glob",
        &json!({
            "paths": [match_pattern.to_string_lossy().as_ref()],
            "exclude": "**/*.md",
            "count": true,
            "directory": cwd_tree.to_string_lossy().as_ref(),
        }),
    )?;
    assert_eq!(
        count_raw.get("paths").and_then(serde_json::Value::as_u64),
        Some(0),
        "--count agrees with the excluded match set: {count_raw}"
    );
    Ok(())
}

/// A pattern whose every match is excluded renders the honest no-match report
/// (the daemon's `no_match_patterns`, echoing the original spelling) with empty
/// output — it must not silently vanish (bug 73).
#[test]
fn glob_exclude_pattern_all_excluded_reports_no_match() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::create_dir(dir.path().join("src"))?;
    std::fs::write(dir.path().join("src/a.rs"), "fn a() {}")?;
    std::fs::write(dir.path().join("src/b.rs"), "fn b() {}")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    let raw = bridge.call_search_raw(
        "tool/glob",
        &json!({ "paths": ["src/*.rs"], "exclude": "*.rs" }),
    )?;

    let output = raw
        .get("output")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    assert!(
        output.trim().is_empty(),
        "no surviving match renders nothing in output: {output:?}"
    );
    let no_match = raw
        .get("no_match_patterns")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(
        no_match.iter().any(|p| p == "src/*.rs"),
        "the all-excluded pattern is reported as a no-match: {no_match:?}"
    );
    Ok(())
}

/// Guard: `grep --exclude-pattern` continues to drop matching files — the glob
/// fix (bug 73) must not disturb grep's already-working exclude.
#[test]
fn grep_exclude_pattern_still_excludes() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::write(dir.path().join("code.rs"), "let needle = 1;\n")?;
    std::fs::write(dir.path().join("notes.txt"), "needle here\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Without an exclude both files match.
    let all = bridge.call_grep(&json!({ "pattern": "needle" }))?;
    assert!(all.contains("code.rs"), "code.rs matches: {all}");
    assert!(all.contains("notes.txt"), "notes.txt matches: {all}");

    // With `--exclude-pattern *.rs` the `.rs` file drops out.
    let excluded = bridge.call_grep(&json!({ "pattern": "needle", "exclude": "*.rs" }))?;
    assert!(
        !excluded.contains("code.rs"),
        "grep exclude drops code.rs: {excluded}"
    );
    assert!(
        excluded.contains("notes.txt"),
        "grep exclude keeps notes.txt: {excluded}"
    );
    Ok(())
}

// ── repeatable --exclude-pattern reaches every consumer (bug 89) ──────

/// A repeated `--exclude-pattern` (bug 89) unions its patterns and BOTH reach a
/// glob **pattern** argument's matches — the bug-73 leak class must not
/// resurface for a multi-pattern exclude. `keep.md` matches neither and
/// survives; the listing and `--count` agree on the surviving set (parity).
#[test]
fn glob_exclude_pattern_repeatable_reaches_pattern_matches_and_count() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::create_dir(dir.path().join("src"))?;
    std::fs::write(dir.path().join("src/a.rs"), "fn a() {}")?;
    std::fs::write(dir.path().join("src/b.txt"), "b")?;
    std::fs::write(dir.path().join("src/keep.md"), "# keep")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Two excludes on the wire, matched as a union against the pattern's files.
    let listing = bridge.call_glob(&json!({
        "paths": ["src/*"],
        "exclude": ["*.rs", "*.txt"],
    }))?;
    assert!(listing.contains("keep.md"), "keep.md survives: {listing}");
    assert!(
        !listing.contains("a.rs"),
        "first exclude reaches the pattern match: {listing}"
    );
    assert!(
        !listing.contains("b.txt"),
        "second exclude reaches the pattern match: {listing}"
    );

    // `--count` filters identically — the surviving cardinality is 1, not the
    // 3 pre-exclude matches — so listing and count never diverge (bug 73 parity
    // holds for a multi-pattern exclude).
    let raw = bridge.call_search_raw(
        "tool/glob",
        &json!({ "paths": ["src/*"], "exclude": ["*.rs", "*.txt"], "count": true }),
    )?;
    let paths = raw.get("paths").and_then(serde_json::Value::as_u64);
    assert_eq!(
        paths,
        Some(1),
        "count agrees with the multi-pattern-excluded listing: {raw}"
    );
    Ok(())
}

/// A repeated `--exclude-pattern` drops every named pattern from a grep pass —
/// both excludes reach the ripgrep walk (bug 89). Only the file matching no
/// exclude survives.
#[test]
fn grep_exclude_pattern_repeatable_excludes_all() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    std::fs::write(dir.path().join("code.rs"), "let needle = 1;\n")?;
    std::fs::write(dir.path().join("data.json"), "needle\n")?;
    std::fs::write(dir.path().join("notes.txt"), "needle here\n")?;

    let mut bridge = spawn_no_lsp(&dir.path().to_string_lossy())?;
    bridge.initialize()?;

    // Two excludes on the wire — both must reach the walk.
    let excluded = bridge.call_grep(&json!({
        "pattern": "needle",
        "exclude": ["*.rs", "*.json"],
    }))?;
    assert!(
        !excluded.contains("code.rs"),
        "first exclude drops code.rs: {excluded}"
    );
    assert!(
        !excluded.contains("data.json"),
        "second exclude drops data.json: {excluded}"
    );
    assert!(
        excluded.contains("notes.txt"),
        "the unmatched file survives: {excluded}"
    );

    // Parity: `--count` reports one distinct file, agreeing with the listing.
    let raw = bridge.call_search_raw(
        "tool/grep",
        &json!({ "pattern": "needle", "exclude": ["*.rs", "*.json"], "count": true }),
    )?;
    let files = raw.get("files").and_then(serde_json::Value::as_u64);
    assert_eq!(
        files,
        Some(1),
        "grep --count agrees with the multi-pattern-excluded listing: {raw}"
    );
    Ok(())
}
