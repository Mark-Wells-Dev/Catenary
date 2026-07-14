// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration test for diagnostics timing after warmup.
//!
//! Verifies that Catenary's post-warmup grace period correctly catches
//! diagnostics from language servers that only publish diagnostics after a
//! file is opened, not at startup.
//!
//! This reproduces the scenario where:
//! 1. The LSP starts and passes the 10-second warmup without any files
//!    being opened (so `has_published_diagnostics` remains false).
//! 2. The first file is opened after warmup.
//! 3. Without the grace period, `wait_for_diagnostics_update` would
//!    short-circuit and return 0 diagnostics.

mod common;

use anyhow::{Context, Result};
use std::time::Duration;

use common::BridgeProcess;

// The `mockls-event` persona is the blessed base (diagnostics-debt 04c):
// renaming the key to a manifest persona makes the mock a diagnostics source
// (replacing the retired bless-list wildcard) while its empty
// behavior bundle keeps the wire behavior identical.
const MOCK_LANG_A: &str = "mockls-event";

#[test]
fn test_diagnostics_on_first_open_past_warmup() -> Result<()> {
    // 1. Create workspace with a test file
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file_path, "echo hello\n")?;

    // 2. Start Catenary with mockls using --publish-version (versioned diagnostics)
    let lsp = common::mockls_lsp_arg(MOCK_LANG_A, "--publish-version");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // 3. Wait past the warmup period.
    //
    // During this time the LSP is running but has no open files,
    // so it never publishes diagnostics. After warmup expires,
    // has_published_diagnostics is still false.
    // Wait long enough that the server is past any early-spawn window.
    std::thread::sleep(Duration::from_secs(11));

    // 4. Request diagnostics via start_editing → accumulate → done_editing MCP.
    //
    // This is the first file interaction. Without the post-warmup grace
    // period, wait_for_diagnostics_update would short-circuit and return
    // empty because has_published_diagnostics is false.
    let text = bridge.call_diagnostics(file_path.to_str().context("file path")?)?;

    // 5. Verify the response contains diagnostics.
    //
    // mockls with --publish-version publishes diagnostics after didOpen,
    // so the grace period should catch them.
    assert!(
        text.contains("mock diagnostic") || text.contains("mockls"),
        "Expected mock diagnostics from mockls, got: {text}"
    );

    Ok(())
}
