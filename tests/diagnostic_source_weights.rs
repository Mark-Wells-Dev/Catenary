// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for cross-feeder diagnostic weight reconciliation
//! (misc 115, bug 42; weight model in linters ticket 05).
//!
//! mockls cannot reproduce rust-analyzer's macro engine, so these tests drive a
//! mockls instance configured with `--extra-diagnostic` (multiple `source`s in
//! one per-file publish) and assert the reconciliation — **union → cross-source
//! dedup (heaviest-weight keeper) → provisional drop** — using the seeded
//! rust-analyzer/flycheck weight default (rust-analyzer native `10`, rustc/clippy
//! `100`, provisional `^E[0-9]+$`). The seed ships in code, so no precedence/weight
//! config is written; the only override is lowering mockls's own harness
//! diagnostic below the native weight (via a `[lsp.server.*.sources]` sub-table) so it
//! does not challenge the native preview cases.

mod common;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use common::BridgeProcess;

const MOCK_LANG: &str = "pR3cD";

/// Writes a config that defines a mockls server for `MOCK_LANG` with the given
/// extra-diagnostic args.
///
/// The seeded rust-analyzer/flycheck weights apply with no config. mockls always
/// emits its own `source: "mockls"` diagnostic alongside the extras; a
/// `[lsp.server.*.sources]` sub-table pins that source's weight below rust-analyzer's
/// native `10` so the harness diagnostic never *challenges* a provisional native
/// preview (it is the heavier-source challenge, not the preview, under test).
fn write_config(dir: &Path, extra_diagnostics: &[&str]) -> Result<PathBuf> {
    let mockls_bin = env!("CARGO_BIN_EXE_mockls");
    let config_path = dir.join("config.toml");

    let mut args = vec![
        format!("\"{MOCK_LANG}\""),
        "\"--log-pid-suffix\"".to_string(),
    ];
    for spec in extra_diagnostics {
        args.push("\"--extra-diagnostic\"".to_string());
        args.push(format!("\"{spec}\""));
    }
    let args_line = args.join(", ");

    std::fs::write(
        &config_path,
        format!(
            "[lsp.server.mockls-event]\n\
             path = \"{mockls_bin}\"\n\
             args = [{args_line}]\n\n\
             [lsp.server.mockls-event.sources]\n\
             mockls = 1\n\n\
             [lsp.language.{MOCK_LANG}]\n\
             servers = [\"mockls-event\"]\n"
        ),
    )?;
    Ok(config_path)
}

/// Native source emits an `E####` a heavier source does NOT corroborate →
/// dropped. The heavier `E####` and the out-of-band `mockls` diagnostic are kept.
#[test]
fn provisional_e_code_dropped_when_heavier_source_reports() -> Result<()> {
    let extras = [
        "rust-analyzer|E0107|phantom arity error at macro site",
        "rustc|E0599|no method named foo",
    ];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("E0599"),
        "heavier rustc diagnostic should be kept. Got:\n{text}"
    );
    assert!(
        !text.contains("E0107"),
        "provisional native E#### should be dropped once flycheck reported. Got:\n{text}"
    );
    assert!(
        text.contains("mock diagnostic"),
        "out-of-band mockls diagnostic should be kept. Got:\n{text}"
    );

    Ok(())
}

/// Heavier-source-only `E####` (no native counterpart) → kept.
#[test]
fn heavier_source_only_e_code_kept() -> Result<()> {
    let extras = ["rustc|E0599|no method named foo"];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("E0599"),
        "heavier-source-only E#### should be kept. Got:\n{text}"
    );

    Ok(())
}

/// No heavier source present (single-source server: only the native source
/// publishes an `E####`) → kept. Absence of a heavier report is not a challenge;
/// no over-suppression of the pre-flycheck preview.
#[test]
fn provisional_kept_when_unchallenged() -> Result<()> {
    let extras = ["rust-analyzer|E0107|phantom arity error at macro site"];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("E0107"),
        "native E#### should be kept when no heavier source reported. Got:\n{text}"
    );

    Ok(())
}

/// The native source publishes only its `E####` preview for this file (no heavier
/// source has reported for it) → kept. Reconciliation runs per-file on the file's
/// own merged set, so a heavier report on a different file does not challenge here.
#[test]
fn provisional_kept_when_heavier_not_yet_reported_for_file() -> Result<()> {
    let extras = ["rust-analyzer|E0308|mismatched types (pre-flycheck preview)"];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("E0308"),
        "native E#### should be kept while no heavier source reported for this \
         file. Got:\n{text}"
    );

    Ok(())
}

/// The same finding (same code, same line) from two sources collapses to the
/// heavier source's copy — the cross-source dedup keeper (ticket 05). Both extras
/// land at line 0; the out-of-band `W0001` code is not provisional, so only dedup
/// applies.
#[test]
fn cross_source_duplicate_collapses_to_heavier() -> Result<()> {
    let extras = [
        "rust-analyzer|W0001|lint preview",
        "clippy|W0001|clippy::needless lint",
    ];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("clippy(W0001)"),
        "heavier clippy copy should survive dedup. Got:\n{text}"
    );
    assert!(
        !text.contains("rust-analyzer(W0001)"),
        "lighter rust-analyzer copy should be collapsed away. Got:\n{text}"
    );

    Ok(())
}
