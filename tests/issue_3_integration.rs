// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Regression test for issue #3: LSP diagnostics timing.
//!
//! Verifies that Catenary correctly waits for the LSP server to complete
//! its analysis after a file change before returning diagnostics,
//! ensuring accuracy and avoiding race conditions.
//!
//! Uses mockls + mockc to deterministically simulate the flycheck pattern
//! (LSP sleeping while subprocess burns CPU), replacing the original
//! rust-analyzer test that was flaky under load.

mod common;

use anyhow::{Context, Result};

use common::BridgeProcess;

// The `mockls-event` persona is the blessed base (diagnostics-debt 04c):
// renaming the key to a manifest persona makes the mock a diagnostics source
// (replacing the retired bless-list wildcard) while its empty
// behavior bundle keeps the wire behavior identical.
const MOCK_LANG_A: &str = "mockls-event";

/// Simulates the flycheck pattern: file change → diagnostics request.
///
/// mockls with `--publish-version --advertise-save --flycheck-command` wraps
/// the subprocess in a `$/progress` bracket. Catenary's `TokenMonitor` waits
/// for the progress cycle to complete before returning diagnostics.
///
/// mockc `--ticks 5` burns 5 centiseconds (~50ms) of CPU — enough to
/// exercise the scheduling pattern without slowing tests.
#[test]
fn test_lsp_diagnostics_waits_for_analysis_after_change() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let file_path = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file_path, "echo hello\n")?;

    let mockc_bin = env!("CARGO_BIN_EXE_mockc");
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    // mockc defaults to --ticks 10 (~100ms CPU). No extra args needed,
    // avoiding quoting issues with Catenary's whitespace-split CATENARY_SERVERS parser.
    let lsp = format!(
        "{MOCK_LANG_A}:{mockls_bin} {MOCK_LANG_A} --publish-version --advertise-save \
         --flycheck-command {mockc_bin}"
    );

    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // First diagnostics call — opens the file, triggers didOpen diagnostics
    let text = bridge.call_diagnostics(file_path.to_str().context("file path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "Initial diagnostics should contain mock diagnostic, got: {text}"
    );

    // Change the file on disk — simulates the agent editing the file
    std::fs::write(&file_path, "echo changed\necho line3\n")?;

    // Second diagnostics call IMMEDIATELY after change.
    // This triggers didChange + didSave. The flycheck subprocess (mockc)
    // runs under a progress bracket. Catenary should wait for the full
    // Active→Idle cycle before returning diagnostics.
    let text = bridge.call_diagnostics(file_path.to_str().context("file path")?)?;
    assert!(
        text.contains("mock diagnostic"),
        "Post-change diagnostics should contain mock diagnostic (after flycheck), got: {text}"
    );

    Ok(())
}
