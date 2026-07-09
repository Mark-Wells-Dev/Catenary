// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Cross-feeder diagnostic source weights + provisional bands (linters ticket
//! 05).
//!
//! Replaces the ticket-02 `DiagnosticPrecedence` priority chain. Reconciliation
//! is now **union → cross-source dedup (heaviest-weight keeper) → provisional
//! drop**. A *weight* expresses per-source trust (higher = more trusted): the
//! dedup pass keeps the heaviest source's copy of a duplicated finding, and a
//! narrow *provisional* band drops a low-trust finding only when a strictly
//! heavier source reported for the file without corroborating it (the misc-115
//! rust-analyzer-vs-flycheck phantom).
//!
//! Weights are **co-located on the source definition** (`[lsp.server.*]` /
//! `[linter.rule.*]`): a definition `weight` is the fallback for the source it emits
//! natively, a `[lsp.server.<name>.sources]` sub-table overrides individual
//! sub-sources (rust-analyzer's `rustc`/`clippy` flycheck sources), and a
//! `provisional` regex marks the native source's provisional code band. The
//! shipped rust-analyzer/flycheck default is seeded **in code**
//! ([`DiagnosticWeights::rust_analyzer_default`]) so it survives a user
//! redefining `[lsp.server.rust-analyzer]`.

use std::collections::HashMap;

use regex::Regex;

use super::linter::LinterConfig;
use super::server::ServerDef;

/// Weight assigned to any source not explicitly listed (linters ticket 05).
///
/// Above rust-analyzer's native `10` (native analysis is the least-trusted
/// source) and below flycheck's `100`.
pub const BASELINE_WEIGHT: u32 = 50;

/// Resolved per-root diagnostic source weights and provisional bands.
///
/// Built by
/// [`LspClientManager::effective_weights`](crate::lsp::LspClientManager::effective_weights)
/// from the seeded code default overlaid with a root's effective `[lsp.server.*]` /
/// `[linter.rule.*]` definitions. Consumed by the `catenary diagnostics` cross-feeder
/// reconciliation.
#[derive(Debug, Clone)]
pub struct DiagnosticWeights {
    /// Source name → trust weight (higher = more trusted).
    weights: HashMap<String, u32>,
    /// Source name → compiled provisional code-band regex. A finding from this
    /// source whose `code` matches the band is provisional.
    provisional: HashMap<String, Regex>,
}

impl DiagnosticWeights {
    /// The shipped default: rust-analyzer's native analysis (`10`) is outweighed
    /// by its flycheck `rustc`/`clippy` ground truth (`100`), and native
    /// `E####` findings are provisional (misc 115, bug 42).
    ///
    /// Seeded in code (not `defaults/servers.toml`) so it survives a user
    /// redefining `[lsp.server.rust-analyzer]` with, e.g., a custom command. Keyed on
    /// the LSP `source` field, so it is inert for any root whose diagnostics
    /// carry none of those sources.
    #[must_use]
    pub fn rust_analyzer_default() -> Self {
        let mut weights = HashMap::new();
        weights.insert("rust-analyzer".to_string(), 10);
        weights.insert("rustc".to_string(), 100);
        weights.insert("clippy".to_string(), 100);
        let mut provisional = HashMap::new();
        // Compile-time-constant pattern; on the impossible compile error ship no
        // provisional band rather than crashing — dedup still runs.
        if let Ok(re) = Regex::new("^E[0-9]+$") {
            provisional.insert("rust-analyzer".to_string(), re);
        }
        Self {
            weights,
            provisional,
        }
    }

    /// The trust weight of `source`, falling back to [`BASELINE_WEIGHT`] for an
    /// unlisted source.
    #[must_use]
    pub fn weight(&self, source: &str) -> u32 {
        self.weights.get(source).copied().unwrap_or(BASELINE_WEIGHT)
    }

    /// Whether a finding from `source` with rendered `code` falls in that
    /// source's provisional band. `false` when the source has no band.
    #[must_use]
    pub fn is_provisional(&self, source: &str, code: &str) -> bool {
        self.provisional
            .get(source)
            .is_some_and(|re| re.is_match(code))
    }

    /// Overlays a `[lsp.server.<name>]` definition's weights onto the set.
    ///
    /// The definition `weight` is the fallback for the native source (named after
    /// the definition); each `[lsp.server.<name>.sources]` entry overrides an
    /// individual sub-source; `provisional` compiles into the native source's
    /// band. Absent fields leave the seeded/earlier values untouched, so a user
    /// redefining `[lsp.server.rust-analyzer]` without weight fields keeps the seeded
    /// default.
    pub fn apply_server_def(&mut self, name: &str, def: &ServerDef) {
        if let Some(weight) = def.weight {
            self.weights.insert(name.to_string(), weight);
        }
        for (source, weight) in &def.sources {
            self.weights.insert(source.clone(), *weight);
        }
        if let Some(pattern) = &def.provisional
            && let Ok(re) = Regex::new(pattern)
        {
            // Validated at config load; on the impossible error drop the band.
            self.provisional.insert(name.to_string(), re);
        }
    }

    /// Overlays a `[linter.rule.<name>]` definition's weight onto the set.
    ///
    /// A linter is a 1:1 emitter (source name == definition name), so only the
    /// fallback `weight` applies.
    pub fn apply_linter(&mut self, name: &str, def: &LinterConfig) {
        if let Some(weight) = def.weight {
            self.weights.insert(name.to_string(), weight);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn unlisted_source_gets_baseline() {
        let w = DiagnosticWeights::rust_analyzer_default();
        assert_eq!(w.weight("some-linter"), BASELINE_WEIGHT);
    }

    #[test]
    fn rust_analyzer_default_weights_and_band() {
        let w = DiagnosticWeights::rust_analyzer_default();
        // Native analysis is the least-trusted source; flycheck outranks it.
        assert_eq!(w.weight("rust-analyzer"), 10);
        assert_eq!(w.weight("rustc"), 100);
        assert_eq!(w.weight("clippy"), 100);
        // Provisional band is rust-analyzer's native E#### codes only.
        assert!(w.is_provisional("rust-analyzer", "E0107"));
        assert!(!w.is_provisional("rust-analyzer", "unused-variable"));
        assert!(!w.is_provisional("rustc", "E0107"), "band is per-source");
    }

    #[test]
    fn apply_server_def_overrides_native_sub_sources_and_band() {
        let mut w = DiagnosticWeights::rust_analyzer_default();
        let def = ServerDef {
            weight: Some(5),
            sources: HashMap::from([("rustc".to_string(), 80)]),
            provisional: Some("^X[0-9]+$".to_string()),
            ..ServerDef::default()
        };
        w.apply_server_def("rust-analyzer", &def);

        // Native source picks up the def fallback.
        assert_eq!(w.weight("rust-analyzer"), 5);
        // Sub-source override wins; an un-overridden seeded sub-source stays.
        assert_eq!(w.weight("rustc"), 80);
        assert_eq!(w.weight("clippy"), 100);
        // The provisional band is replaced for the native source.
        assert!(w.is_provisional("rust-analyzer", "X9"));
        assert!(!w.is_provisional("rust-analyzer", "E0107"));
    }

    #[test]
    fn apply_server_def_without_weight_keeps_seeded_default() {
        // A user redefining the server def only (e.g. a `path` override, no
        // weight fields) must not erase the seeded rust-analyzer/flycheck weights.
        let mut w = DiagnosticWeights::rust_analyzer_default();
        let def = ServerDef {
            path: Some("/opt/my-rust-analyzer".to_string()),
            ..ServerDef::default()
        };
        w.apply_server_def("rust-analyzer", &def);
        assert_eq!(w.weight("rust-analyzer"), 10);
        assert_eq!(w.weight("rustc"), 100);
        assert!(w.is_provisional("rust-analyzer", "E0107"));
    }

    #[test]
    fn apply_linter_sets_source_weight() {
        let mut w = DiagnosticWeights::rust_analyzer_default();
        let mut lc = LinterConfig::new("shellcheck", vec![], vec![]).expect("compile");
        lc.weight = Some(70);
        w.apply_linter("shellcheck", &lc);
        assert_eq!(w.weight("shellcheck"), 70);
    }
}
