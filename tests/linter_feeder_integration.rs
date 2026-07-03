// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! End-to-end integration tests for the standalone-linter feeder (linters
//! tickets 01/02/05).
//!
//! The adapters and the weight reconciliation are otherwise covered only by unit
//! tests against hand-built strings / `FeederEntry`s. These drive the **real**
//! `LinterFeeder` subprocess path — spawn → parse → route → render — and the
//! cross-feeder merge, using the hermetic `mocklint` tool (ticket 06) in place of
//! any installed linter. `mocklint` emits canned findings in a chosen adapter's
//! output shape for the file paths it is handed, so a `[linter.rule.<name>]` whose
//! `command` points at it exercises the same path a real shellcheck/SARIF tool
//! would, without depending on the binary being present.

mod common;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use common::BridgeProcess;

/// Mock language id for the cross-feeder test (doubles as the file extension,
/// matching `mockls`'s `--scan-roots`/classification convention).
const MOCK_LANG: &str = "lZ9kq";

/// Joins TOML string-array elements (`["a", "b"]`) from a slice.
fn toml_array(items: &[&str]) -> String {
    items
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Writes a config with a single `[linter.rule.<name>]` pointing `command` at
/// `mocklint`. No LSP server is configured, so the file is covered by lint alone.
fn write_linter_config(
    dir: &Path,
    name: &str,
    args: &[&str],
    patterns: &[&str],
) -> Result<PathBuf> {
    let mocklint = env!("CARGO_BIN_EXE_mocklint");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[linter.rule.{name}]\n\
             command = \"{mocklint}\"\n\
             args = [{}]\n\
             patterns = [{}]\n",
            toml_array(args),
            toml_array(patterns),
        ),
    )?;
    Ok(config_path)
}

/// A standalone `[linter.rule.shellcheck]` (no LSP server) renders its finding through
/// the real spawn → parse (shellcheck `json1`) → route → render path.
#[test]
fn linter_only_shellcheck_renders() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join("build.sh"), "echo $HOME\n")?;
        write_linter_config(
            root,
            "shellcheck",
            &[
                "--format",
                "shellcheck",
                "--diag",
                "SC2086|1|6|Double quote to prevent globbing",
            ],
            &["**/*.sh"],
        )
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join("build.sh");
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("shellcheck(SC2086)"),
        "lint-only shellcheck finding should render. Got:\n{text}"
    );
    Ok(())
}

/// A non-blessed linter name falls to the generic SARIF adapter; the source comes
/// from the SARIF `tool.driver.name` (`--source`).
#[test]
fn linter_only_sarif_renders() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join("Dockerfile"), "FROM alpine\nRUN apk add curl\n")?;
        write_linter_config(
            root,
            "hadolint",
            &[
                "--format",
                "sarif",
                "--source",
                "hadolint",
                "--diag",
                "DL3018|2|1|Pin versions in apk add",
            ],
            &["Dockerfile", "**/Dockerfile"],
        )
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join("Dockerfile");
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("hadolint(DL3018)"),
        "lint-only SARIF finding should render with its tool.driver.name source. \
         Got:\n{text}"
    );
    Ok(())
}

/// Exit status is not failure: a linter that exits nonzero (as real linters do
/// when they find issues) still has its parseable output rendered.
#[test]
fn fail_soft_nonzero_exit_still_renders() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join("build.sh"), "echo $HOME\n")?;
        write_linter_config(
            root,
            "shellcheck",
            &[
                "--format",
                "shellcheck",
                "--exit-code",
                "1",
                "--diag",
                "SC2086|1|6|Double quote to prevent globbing",
            ],
            &["**/*.sh"],
        )
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join("build.sh");
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("shellcheck(SC2086)"),
        "a nonzero exit status must not suppress parseable findings. Got:\n{text}"
    );
    Ok(())
}

/// A linter emitting malformed output is dropped with the batch intact: a second,
/// valid linter on the same file still renders, and the garbage never leaks.
#[test]
fn fail_soft_malformed_output_dropped() -> Result<()> {
    let mocklint = env!("CARGO_BIN_EXE_mocklint");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join("build.sh"), "echo $HOME\n")?;
        let config_path = root.join("config.toml");
        // Two linters over the same file: `shellcheck` emits a valid finding,
        // `broken` (→ SARIF adapter) emits malformed JSON. The malformed one is
        // dropped + warned; the batch survives and the valid finding renders.
        std::fs::write(
            &config_path,
            format!(
                "[linter.rule.shellcheck]\n\
                 command = \"{mocklint}\"\n\
                 args = [\"--format\", \"shellcheck\", \"--diag\", \"SC2086|1|6|Double quote\"]\n\
                 patterns = [\"**/*.sh\"]\n\n\
                 [linter.rule.broken]\n\
                 command = \"{mocklint}\"\n\
                 args = [\"--raw\", \"{{not json\"]\n\
                 patterns = [\"**/*.sh\"]\n",
            ),
        )?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join("build.sh");
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("shellcheck(SC2086)"),
        "the valid linter must still render alongside a malformed one. Got:\n{text}"
    );
    assert!(
        !text.contains("not json"),
        "malformed linter output must be dropped, never leaked. Got:\n{text}"
    );
    Ok(())
}

/// Writes a config wiring both an LSP feeder (`mockls`, emitting its native
/// diagnostic plus an out-of-band `shellcheck|SC2086` extra) and a
/// `[linter.rule.shellcheck]` feeder (`mocklint`, emitting `SC2086` + `SC2148`) over
/// the same `MOCK_LANG` file — the bash-language-server-wrapping-shellcheck
/// scenario the workstream is built around.
fn write_cross_feeder_config(dir: &Path) -> Result<PathBuf> {
    let mockls = env!("CARGO_BIN_EXE_mockls");
    let mocklint = env!("CARGO_BIN_EXE_mocklint");
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[lsp.server.mockls-{MOCK_LANG}]\n\
             command = \"{mockls}\"\n\
             args = [\"{MOCK_LANG}\", \"--log-pid-suffix\", \
             \"--extra-diagnostic\", \"shellcheck|SC2086|wrapped by language server\"]\n\n\
             [lsp.language.{MOCK_LANG}]\n\
             servers = [\"mockls-{MOCK_LANG}\"]\n\n\
             [linter.rule.shellcheck]\n\
             command = \"{mocklint}\"\n\
             args = [\"--format\", \"shellcheck\", \
             \"--diag\", \"SC2086|1|1|Double quote to prevent globbing\", \
             \"--diag\", \"SC2148|2|1|Add a shebang\"]\n\
             patterns = [\"**/*.{MOCK_LANG}\"]\n"
        ),
    )?;
    Ok(config_path)
}

/// The marquee case: a file covered by both an LSP feeder and a linter feeder,
/// both reporting `SC2086` at the same line, collapses to **one** entry
/// (cross-source dedup, ticket 05) while the linter-only `SC2148` and the LSP
/// native diagnostic both survive — proving both feeders ran and only the overlap
/// deduped.
#[test]
fn cross_feeder_overlap_dedups_others_survive() -> Result<()> {
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_cross_feeder_config(root)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert_eq!(
        text.matches("SC2086").count(),
        1,
        "the overlapping SC2086 from both feeders should collapse to one. Got:\n{text}"
    );
    // Both copies carry source `shellcheck` (the wrap preserves identity), so the
    // dedup is an equal-weight tie → first-seen wins, and the LSP feeder is seen
    // before the linter. The surviving copy is therefore the LSP one: its message
    // is present, the linter's SC2086 message is collapsed away.
    assert!(
        text.contains("wrapped by language server"),
        "the first-seen (LSP) copy should win the equal-weight tie. Got:\n{text}"
    );
    assert!(
        !text.contains("Double quote to prevent globbing"),
        "the linter's equal-weight SC2086 copy should be collapsed away. Got:\n{text}"
    );
    assert!(
        text.contains("SC2148"),
        "the linter-only SC2148 should survive the merge. Got:\n{text}"
    );
    assert!(
        text.contains("mock diagnostic"),
        "the LSP native diagnostic should survive the merge. Got:\n{text}"
    );
    Ok(())
}

/// Cross-feeder **weight** discrimination: when the LSP feeder and the linter
/// feeder report the same `(code, line)` from *different* sources, the
/// heavier-weight source's copy is the dedup keeper — even though the LSP feeder
/// is seen first (which would win an equal-weight tie). The linter is pinned
/// heavier (`weight = 90`) than the LSP feeder's distinct source (baseline `50`),
/// so the linter's copy survives and the LSP copy is dropped, proving the weight
/// keeper overrides feeder order across two real feeders (the integration-tier
/// counterpart to the `dedup_collapses_across_sources_keeping_heaviest` unit
/// test).
#[test]
fn cross_feeder_heavier_source_wins_over_first_seen() -> Result<()> {
    let mockls = env!("CARGO_BIN_EXE_mockls");
    let mocklint = env!("CARGO_BIN_EXE_mocklint");
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[lsp.server.mockls-{MOCK_LANG}]\n\
                 command = \"{mockls}\"\n\
                 args = [\"{MOCK_LANG}\", \"--log-pid-suffix\", \
                 \"--extra-diagnostic\", \"native-analysis|SC2086|server-side preview\"]\n\n\
                 [lsp.language.{MOCK_LANG}]\n\
                 servers = [\"mockls-{MOCK_LANG}\"]\n\n\
                 [linter.rule.shellcheck]\n\
                 command = \"{mocklint}\"\n\
                 args = [\"--format\", \"shellcheck\", \"--diag\", \"SC2086|1|1|standalone finding\"]\n\
                 patterns = [\"**/*.{MOCK_LANG}\"]\n\
                 weight = 90\n"
            ),
        )?;
        Ok(config_path)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("shellcheck(SC2086)"),
        "the heavier linter source should be the dedup keeper. Got:\n{text}"
    );
    assert!(
        !text.contains("native-analysis(SC2086)"),
        "the lighter LSP source's copy should be collapsed away despite being \
         seen first. Got:\n{text}"
    );
    Ok(())
}
