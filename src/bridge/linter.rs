// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The daemon-side linter feeder adapter.
//!
//! The second diagnostic feeder behind the [`DiagnosticFeeder`] port
//! (workstream 34 ticket 01).
//!
//! Since ws43-04 the linter CORE — spawn discipline, output parsing, severity
//! mapping, routing rules — lives in the shared, daemon-free [`crate::linter`]
//! module (the CLI query sink calls it directly, with no daemon at all). This
//! adapter is what remains daemon-side: it binds the core to the daemon's
//! registered-roots ledger ([`FilesystemManager`]) and effective-config view
//! ([`LspClientManager`]) for the diagnostics batch, and maps each typed
//! [`LinterRunOutcome`] onto the daemon's degrade surface (a `warn!` into the
//! firehose; fail-soft, never a poisoned batch).

use std::collections::BTreeMap;
use std::path::PathBuf;

use tracing::warn;

use crate::bridge::filesystem_manager::FilesystemManager;
use crate::linter::{FeederDiagnostics, LinterRunOutcome, run_linter};
use crate::lsp::LspClientManager;

/// Port: a feeder that publishes LSP-shaped diagnostics for a set of files.
///
/// The LSP client is the protocol-native feeder (not refactored onto this
/// trait, to avoid a risky rewrite of the diagnostics path); [`LinterFeeder`] is
/// the subprocess-and-parse adapter that runs standalone linters.
#[allow(
    async_fn_in_trait,
    reason = "single in-process adapter; the future is awaited in place in the diagnostics batch, never spawned across threads"
)]
pub trait DiagnosticFeeder {
    /// Produces LSP-shaped diagnostics for `files`.
    ///
    /// Fail-soft: an absent linter is skipped, a parse failure drops that
    /// linter's diagnostics, and the returned set carries whatever succeeded.
    async fn feed(&self, files: &[PathBuf]) -> Vec<FeederDiagnostics>;
}

/// The standalone-linter adapter behind the [`DiagnosticFeeder`] port.
///
/// Borrows the shared [`LspClientManager`] (effective linter set + per-root
/// `disable_lint`) and [`FilesystemManager`] (root resolution) for the duration
/// of one diagnostics batch. The subprocess run and output translation are the
/// shared core's ([`crate::linter::run_linter`]).
pub struct LinterFeeder<'a> {
    manager: &'a LspClientManager,
    fs: &'a FilesystemManager,
}

impl<'a> LinterFeeder<'a> {
    /// Builds a feeder over the shared managers for one diagnostics batch.
    pub const fn new(manager: &'a LspClientManager, fs: &'a FilesystemManager) -> Self {
        Self { manager, fs }
    }
}

impl DiagnosticFeeder for LinterFeeder<'_> {
    async fn feed(&self, files: &[PathBuf]) -> Vec<FeederDiagnostics> {
        // Group the batch by owning root: routing globs and the effective linter
        // set are both per-root.
        let mut by_root: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
        for file in files {
            if let Some(root) = self.fs.resolve_root(file) {
                by_root.entry(root).or_default().push(file.clone());
            }
        }

        let mut out = Vec::new();
        for (root, root_files) in by_root {
            if self.manager.is_lint_disabled(&root) {
                continue;
            }
            let linters = self.manager.effective_linters(&root);
            // Deterministic linter order for stable output.
            let mut names: Vec<&String> = linters.keys().collect();
            names.sort();
            for name in names {
                let Some(linter) = linters.get(name) else {
                    continue;
                };
                if linter.disable || linter.command.is_empty() {
                    continue;
                }
                let matching: Vec<PathBuf> = root_files
                    .iter()
                    .filter(|f| {
                        f.strip_prefix(&root)
                            .is_ok_and(|rel| self.fs.linter_routes(linter, f, rel))
                    })
                    .cloned()
                    .collect();
                if matching.is_empty() {
                    continue;
                }
                // Map the typed outcome onto the daemon's degrade surface —
                // fail-soft: a not-installed linter, a spawn error, or a parse
                // failure each yields an empty feed (after one warn) rather
                // than propagating; its files stay unverified, never falsely
                // clean.
                match run_linter(name, linter, &root, &matching).await {
                    LinterRunOutcome::Completed(feeds) => out.extend(feeds),
                    LinterRunOutcome::NotInstalled => {
                        // Not installed → ONE notify, skip (not a hard error).
                        warn!(
                            linter = %name,
                            command = %linter.command,
                            "linter '{name}' not found ({}); skipping — install it or set \
                             [linter.rule.{name}] disable = true",
                            linter.command,
                        );
                    }
                    LinterRunOutcome::SpawnFailed(e) => {
                        warn!(
                            linter = %name,
                            "linter '{name}' failed to run: {e}; skipping"
                        );
                    }
                    LinterRunOutcome::ParseFailed(e) => {
                        // Parse failure → drop this linter's diagnostics + warn,
                        // never poison the batch.
                        warn!(
                            linter = %name,
                            "linter '{name}' output parse failed: {e}; dropping its diagnostics",
                        );
                    }
                }
            }
        }
        out
    }
}
