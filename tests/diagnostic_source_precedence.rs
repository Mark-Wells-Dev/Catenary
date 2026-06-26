// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Integration tests for generic diagnostic source-precedence reconciliation
//! (misc 115, bug 42).
//!
//! mockls cannot reproduce rust-analyzer's macro engine, so these tests drive a
//! mockls instance configured with `--extra-diagnostic` (multiple `source`s in
//! one per-file publish) plus a `[[diagnostics.precedence]]` chain (per-root,
//! cross-feeder — linters ticket 02), and assert the filter drops a
//! lower-priority source's diagnostics in the band — but only once a
//! higher-priority source has reported.

mod common;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use common::BridgeProcess;

const MOCK_LANG: &str = "pR3cD";

/// Writes a config that defines a mockls server for `MOCK_LANG` with the given
/// extra-diagnostic args and the rust-analyzer-style precedence chain
/// (rustc/clippy outrank rust-analyzer, scoped to the rustc `E####` band) as a
/// `[[diagnostics.precedence]]` section. When `with_policy` is false, an
/// explicit empty `[diagnostics]` / `precedence = []` clears the shipped default
/// so the baseline (union) is observed — the global default would otherwise
/// reconcile these very sources.
fn write_config(dir: &Path, extra_diagnostics: &[&str], with_policy: bool) -> Result<PathBuf> {
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

    // The precedence chain (array-of-tables under [diagnostics]), or an explicit
    // empty array to clear the shipped default. Placed first so the section is
    // not captured by a following `[table]` header.
    let precedence = if with_policy {
        "[[diagnostics.precedence]]\n\
         priority = [\"rustc\", \"clippy\", \"rust-analyzer\"]\n\
         code_pattern = \"^E[0-9]+$\"\n\n"
            .to_string()
    } else {
        "[diagnostics]\nprecedence = []\n\n".to_string()
    };

    std::fs::write(
        &config_path,
        format!(
            "{precedence}\
             [server.mockls-{MOCK_LANG}]\n\
             command = \"{mockls_bin}\"\n\
             args = [{args_line}]\n\n\
             [language.{MOCK_LANG}]\n\
             servers = [\"mockls-{MOCK_LANG}\"]\n"
        ),
    )?;
    Ok(config_path)
}

/// Advisory source emits an `E####` the authoritative source does NOT
/// corroborate → dropped. The authoritative `E####` and the out-of-policy
/// `mockls` diagnostic are kept.
#[test]
fn advisory_e_code_dropped_when_authoritative_reports() -> Result<()> {
    let extras = [
        "rust-analyzer|E0107|phantom arity error at macro site",
        "rustc|E0599|no method named foo",
    ];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras, true)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("E0599"),
        "authoritative rustc diagnostic should be kept. Got:\n{text}"
    );
    assert!(
        !text.contains("E0107"),
        "advisory native E#### should be dropped once flycheck reported. Got:\n{text}"
    );
    assert!(
        text.contains("mock diagnostic"),
        "out-of-policy mockls diagnostic should be kept. Got:\n{text}"
    );

    Ok(())
}

/// Authoritative-only `E####` (no advisory counterpart) → kept.
#[test]
fn authoritative_only_e_code_kept() -> Result<()> {
    let extras = ["rustc|E0599|no method named foo"];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras, true)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("E0599"),
        "authoritative-only E#### should be kept. Got:\n{text}"
    );

    Ok(())
}

/// No authoritative source present at all (single-source server: the policy is
/// configured but only the advisory source publishes) → advisory kept. Absence
/// of an authoritative report is not contradiction; no over-suppression.
#[test]
fn advisory_kept_when_no_authoritative_present() -> Result<()> {
    let extras = ["rust-analyzer|E0107|phantom arity error at macro site"];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras, true)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("E0107"),
        "advisory E#### should be kept when no authoritative source reported. Got:\n{text}"
    );

    Ok(())
}

/// The policy lists authoritative sources, but for THIS file only the advisory
/// source reported (the authoritative source has not reported for this file) →
/// advisory kept. The reconciliation runs per-file on the file's own merged
/// set, so an authoritative report on a different file does not suppress here.
#[test]
fn advisory_kept_when_authoritative_not_yet_reported_for_file() -> Result<()> {
    // The policy names rustc/clippy as authoritative, but this mockls instance
    // publishes only the advisory native E#### for the file — no flycheck has
    // landed yet for it.
    let extras = ["rust-analyzer|E0308|mismatched types (pre-flycheck preview)"];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras, true)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("E0308"),
        "advisory E#### should be kept while authoritative has not reported for \
         this file. Got:\n{text}"
    );

    Ok(())
}

/// Without a precedence policy, overlapping multi-source diagnostics are the
/// union (the pre-misc-115 behavior) — the advisory `E####` is NOT dropped.
/// Guards against the mechanism firing when no policy opts in.
#[test]
fn no_policy_keeps_union() -> Result<()> {
    let extras = [
        "rust-analyzer|E0107|phantom arity error at macro site",
        "rustc|E0599|no method named foo",
    ];
    let mut bridge = BridgeProcess::spawn_with_config(|root| {
        std::fs::write(root.join(format!("a.{MOCK_LANG}")), "echo hi\n")?;
        write_config(root, &extras, false)
    })?;
    bridge.initialize()?;

    let file = bridge.root_path().join(format!("a.{MOCK_LANG}"));
    let text = bridge.call_diagnostics(file.to_str().context("path")?)?;

    assert!(
        text.contains("E0107") && text.contains("E0599"),
        "without a policy, both sources should survive (union). Got:\n{text}"
    );

    Ok(())
}
