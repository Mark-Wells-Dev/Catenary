// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for the `glob` tool (directory/file/pattern modes).

mod common;

use anyhow::{Context, Result};
use serde_json::json;

use common::BridgeProcess;

const MOCK_LANG_A: &str = "yX4Za";

/// Spawns a bridge with an optional custom LSP arg.
///
/// If `lsp_args` is `None`, uses mockls for `MOCK_LANG_A`.
fn spawn_bridge(root: &str, lsp_args: Option<&str>) -> Result<BridgeProcess> {
    let default_lsp = common::mockls_lsp_arg(MOCK_LANG_A, "");
    let lsp = lsp_args.unwrap_or(&default_lsp);
    BridgeProcess::spawn(&[lsp], root)
}

/// Spawns a bridge with multiple roots and an optional custom LSP arg.
fn spawn_bridge_multi_root(roots: &[&str], lsp_args: Option<&str>) -> Result<BridgeProcess> {
    let default_lsp = common::mockls_lsp_arg(MOCK_LANG_A, "");
    let lsp = lsp_args.unwrap_or(&default_lsp);
    BridgeProcess::spawn_multi_root(&[lsp], roots)
}

#[test]
fn test_glob_directory_basic() -> Result<()> {
    let temp = tempfile::tempdir()?;
    std::fs::write(temp.path().join("file1.txt"), "content")?;
    std::fs::create_dir(temp.path().join("subdir"))?;
    std::fs::write(temp.path().join("subdir/file2.rs"), "fn main() {}")?;

    let mut bridge = spawn_bridge(temp.path().to_str().context("invalid path")?, None)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [temp.path().to_str().context("invalid path")?] }),
    )?;

    assert!(
        text.contains("file1.txt"),
        "Should list file1.txt, got:\n{text}"
    );
    assert!(
        text.contains("subdir/"),
        "Should list subdir/, got:\n{text}"
    );
    Ok(())
}

#[test]
fn test_glob_directory_symbols() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let script = temp.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&script, "struct Config\nenum Mode\nconst MAX_SIZE\n")?;

    let mut bridge = spawn_bridge(temp.path().to_str().context("invalid path")?, None)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [temp.path().to_str().context("invalid path")?] }),
    )?;

    assert!(
        text.contains(&format!("types.{MOCK_LANG_A}")),
        "Should list the file, got:\n{text}"
    );
    // Tier 2: file listing with line counts (no symbols until 08b).
    assert!(
        text.contains("(3 lines)"),
        "Should show line count, got:\n{text}"
    );
    Ok(())
}

/// Verifies that glob returns a line count header for a single file.
///
/// Symbols (defensive maps) are added in 08b. For now, only the
/// header with line count is shown.
#[test]
fn test_glob_file_outline() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let script = temp.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(
        &script,
        "struct Config\nenum Mode\nconst MAX_SIZE\nfn do_work\n",
    )?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let lsp = format!("{MOCK_LANG_A}:{mockls_bin} {MOCK_LANG_A}");

    let mut bridge = spawn_bridge(temp.path().to_str().context("invalid path")?, Some(&lsp))?;
    bridge.initialize()?;

    let text = bridge.call_tool_text(
        "glob",
        &json!({ "paths": [script.to_str().context("file path")?] }),
    )?;

    // Line count header
    assert!(
        text.contains("(4 lines)"),
        "Should show line count, got:\n{text}"
    );

    // Symbols from documentSymbol should appear as a defensive map.
    assert!(
        text.contains("Config"),
        "Should show symbols from documentSymbol, got:\n{text}"
    );
    Ok(())
}

/// Bug #26: a single-file `glob` outline refreshes after a host `Edit`/`Write`
/// with no intervening `catenary diagnostics`. The first glob populates and
/// records the file's mtime; after the file is rewritten on disk, the next
/// glob's `ensure_symbols` detects the newer mtime and re-requests
/// `documentSymbol`, so the renamed symbol replaces the stale one in the
/// outline (the reported symptom: deleted/renamed symbols still listed).
#[test]
fn test_glob_outline_refreshed_after_host_edit() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let script = temp.path().join(format!("types.{MOCK_LANG_A}"));
    std::fs::write(&script, "struct OldName\nfn keep_me\n")?;

    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let lsp = format!("{MOCK_LANG_A}:{mockls_bin} {MOCK_LANG_A}");

    let mut bridge = spawn_bridge(temp.path().to_str().context("invalid path")?, Some(&lsp))?;
    bridge.initialize()?;

    let path = script.to_str().context("file path")?;
    let before = bridge.call_tool_text("glob", &json!({ "paths": [path] }))?;
    assert!(
        before.contains("OldName"),
        "first glob should show the original symbol, got:\n{before}"
    );

    // Rewrite on disk as a host Edit/Write would (rename the struct). Force a
    // strictly-newer mtime so the test is independent of filesystem timestamp
    // resolution; no diagnostics/sed pass runs.
    std::fs::write(&script, "struct NewName\nfn keep_me\n")?;
    {
        let f = std::fs::File::options().write(true).open(&script)?;
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))?;
    }

    let after = bridge.call_tool_text("glob", &json!({ "paths": [path] }))?;
    assert!(
        after.contains("NewName"),
        "glob outline should refresh to the renamed symbol, got:\n{after}"
    );
    assert!(
        !after.contains("OldName"),
        "stale pre-edit symbol must not survive the next glob, got:\n{after}"
    );
    Ok(())
}

/// Bug #26 residual (end-to-end): the paged result cache is checked at the top
/// of the pipeline and is generation-gated, but a host `Edit`/`Write` bumps no
/// generation — so a repeated multi-page glob after editing a listed file would
/// serve a stale cached page. The cache now snapshots the rendered files' mtimes
/// and re-validates them on every page fetch. This also guards the file-set
/// threading from server → `ResultCache::put` against future regressions.
#[test]
fn test_glob_multipage_cache_invalidates_on_host_edit() -> Result<()> {
    let temp = tempfile::tempdir()?;
    // Enough files that the listing spans multiple pages (glob budget is 2000).
    for i in 0..150 {
        std::fs::write(temp.path().join(format!("file_{i:04}.txt")), "only\n")?;
    }

    let mut bridge = spawn_bridge(temp.path().to_str().context("invalid path")?, None)?;
    bridge.initialize()?;

    let dir = temp.path().to_str().context("invalid path")?;

    // First glob caches the multi-page listing. A non-empty page 2 confirms the
    // result is multi-page (single-page results are not cached — which would
    // make this test vacuous).
    let _p1 = bridge.call_tool_text("glob", &json!({ "paths": [dir] }))?;
    let p2 = bridge.call_tool_text("glob", &json!({ "paths": [dir], "page": 2 }))?;
    assert!(
        !p2.trim().is_empty(),
        "listing should span multiple pages (cache active), got empty page 2"
    );

    // Edit the first-sorted file (glob sorts by name → lands on page 1) to a new
    // line count, as a host Edit/Write would. Force a strictly-newer mtime.
    let edited = temp.path().join("file_0000.txt");
    std::fs::write(&edited, "a\nb\nc\nd\ne\nf\ng\nh\n")?;
    {
        let f = std::fs::File::options().write(true).open(&edited)?;
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))?;
    }

    // The next page-1 glob must reflect the new line count — only file_0000 has
    // a non-`1` count, so `(8 lines)` proves the cached page was invalidated by
    // the rendered file's mtime change rather than served stale.
    let after = bridge.call_tool_text("glob", &json!({ "paths": [dir] }))?;
    assert!(
        after.contains("file_0000.txt"),
        "edited file should be on page 1, got:\n{after}"
    );
    assert!(
        after.contains("(8 lines)"),
        "edited file's fresh line count must appear (cache must miss on host edit), got:\n{after}"
    );
    Ok(())
}

/// Bug #26 (add/remove): a file *added* to a globbed multi-page directory must
/// appear on the next identical glob. There's no prior entry to stat, so this is
/// caught by the directory witness — the OS bumps the listed dir's mtime when an
/// entry is added, and the cache re-stats it on `get`.
#[test]
fn test_glob_multipage_cache_invalidates_on_file_added() -> Result<()> {
    let temp = tempfile::tempdir()?;
    for i in 0..150 {
        std::fs::write(temp.path().join(format!("file_{i:04}.txt")), "only\n")?;
    }

    let mut bridge = spawn_bridge(temp.path().to_str().context("invalid path")?, None)?;
    bridge.initialize()?;

    let dir = temp.path().to_str().context("invalid path")?;

    let _p1 = bridge.call_tool_text("glob", &json!({ "paths": [dir] }))?;
    let p2 = bridge.call_tool_text("glob", &json!({ "paths": [dir], "page": 2 }))?;
    assert!(
        !p2.trim().is_empty(),
        "listing should span multiple pages (cache active), got empty page 2"
    );

    // Add a new file that sorts first (lands on page 1). Force a strictly-newer
    // directory mtime so the test doesn't depend on filesystem timestamp
    // resolution.
    std::fs::write(temp.path().join("aaa_new.txt"), "hi\n")?;
    {
        let d = std::fs::File::open(temp.path())?;
        d.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(10))?;
    }

    let after = bridge.call_tool_text("glob", &json!({ "paths": [dir] }))?;
    assert!(
        after.contains("aaa_new.txt"),
        "a newly added file must appear on the next glob (cache must miss on dir change), got:\n{after}"
    );
    Ok(())
}

#[test]
fn test_glob_directory_explicit_path() -> Result<()> {
    // When an explicit path is given, even in multi-root mode, only that path is shown
    let dir_a = tempfile::tempdir()?;
    let dir_b = tempfile::tempdir()?;

    std::fs::write(dir_a.path().join("only_a.txt"), "a")?;
    std::fs::write(dir_b.path().join("only_b.txt"), "b")?;

    let root_a = dir_a.path().to_str().context("invalid path A")?;
    let root_b = dir_b.path().to_str().context("invalid path B")?;

    let mut bridge = spawn_bridge_multi_root(&[root_a, root_b], None)?;
    bridge.initialize()?;

    let text = bridge.call_tool_text("glob", &json!({ "paths": [root_a] }))?;

    assert!(
        text.contains("only_a.txt"),
        "Should contain only_a.txt from explicit path, got:\n{text}"
    );
    assert!(
        !text.contains("only_b.txt"),
        "Should NOT contain only_b.txt when explicit path is root A, got:\n{text}"
    );

    Ok(())
}
