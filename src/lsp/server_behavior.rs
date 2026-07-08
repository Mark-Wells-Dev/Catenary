// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Per-server behavior profiles (misc 157).
//!
//! One engine-internal lookup, [`ServerProfile::for_server`], resolves a server's
//! **conformance settings** — the engine-required invariants Catenary needs to
//! function correctly against that server. These are distinct from the *default
//! configurables* (the shipped [`crate::config::server::ServerDef`] layer in
//! `defaults/servers.toml`): configurables are user/project-overridable through
//! today's config merge; conformance settings resolve through this profile and are
//! applied **after** every user/project merge and are **never** overridable by any
//! config layer. This is the enforcement-keys boundary inverted — enforcement keys
//! are user-scope-only; conformance settings are shipped/engine-scope-only.
//!
//! The knowledge lives in exactly one place — the [`profile`] table below. The
//! consuming seams (client-capability construction, initialization-option
//! assembly, the diagnostics pull gate) each make a single profile call and are
//! themselves server-name-blind: no seam special-cases a server by name. The CI
//! conformance matrix re-verifies every profile invariant on every re-pin, which
//! is what makes carrying these settings in code safe (maintainer direction, bug
//! 82: discipline knowledge is "not something I want to be 'configurable' by the
//! user but set on a case by case basis").
//!
//! Two conformance invariants are cased today:
//!
//! - **rust-analyzer** — [`ServerProfile::suppresses_pull_diagnostics`]. Catenary
//!   is push-first for the Rust family: RA suppressing native pushes when the
//!   client advertises pull would drop warnings/hints the push channel carries (RA
//!   #18709), and the flycheck family (clippy/cargo) is push-only upstream so pull
//!   was never a complete answer. The profile withholds the
//!   `textDocument.diagnostic` client capability *and* gates the client-side pull
//!   path, so RA's native pushes are the sole diagnostic channel — airtight even
//!   if RA spontaneously advertises `diagnosticProvider`.
//! - **gopls** — [`ServerProfile::forced_initialization_options`]. `pullDiagnostics:
//!   true` (real LSP 3.17 pull, advertised since gopls v0.17; our pin v0.22.0 has
//!   it) and `diagnosticsDelay: "0s"` (no debounce blind window — the debounce
//!   coalesces human keystreams, but Catenary sends discrete batches). These are
//!   conformance settings, not `defaults/servers.toml` entries, because a user
//!   `[lsp.server.gopls]` replaces the shipped default wholesale (no field merge —
//!   see `test_builtin_no_merge`), which would silently drop them; and because they
//!   must win over a user who sets them otherwise.

use serde_json::{Value, json};

use crate::config::merge::deep_merge;

/// The resolved conformance profile for one LSP server.
///
/// Built by [`Self::for_server`] from the engine-internal [`profile`] table. All
/// seams consult a `ServerProfile` rather than testing a server name, so the
/// per-server knowledge stays in the table and the seams stay server-name-blind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerProfile {
    /// When set, the server must never receive the `textDocument.diagnostic`
    /// client capability, and must never be sent `textDocument/diagnostic`
    /// (advertised pull *or* best-effort probe) — its native pushes are the sole
    /// diagnostic channel.
    suppress_pull_diagnostics: bool,
    /// Conformance `initializationOptions` overlaid onto (and winning over) the
    /// user-supplied options at initialize time. `None` when the server has no
    /// forced options.
    forced_initialization_options: Option<Value>,
}

impl ServerProfile {
    /// Resolves the conformance profile for `server_name` — the single lookup
    /// every seam calls. Returns the default (no conformance settings) profile for
    /// any server not named in the [`profile`] table.
    #[must_use]
    pub fn for_server(server_name: &str) -> Self {
        profile(server_name)
    }

    /// Whether this server's client-side pull path is gated off.
    ///
    /// A `true` profile withholds the `textDocument.diagnostic` client capability
    /// ([`Self::shape_client_capabilities`]) and suppresses every pull the daemon
    /// would otherwise issue for the server.
    #[must_use]
    pub const fn suppresses_pull_diagnostics(&self) -> bool {
        self.suppress_pull_diagnostics
    }

    /// Applies the profile's client-capability shaping to a built `capabilities`
    /// object in place — the capability-construction seam.
    ///
    /// Today this removes `textDocument.diagnostic` for a pull-suppressed server.
    /// Removing the key (rather than building a different block) keeps the
    /// capability shape byte-for-byte identical for every un-profiled server.
    pub fn shape_client_capabilities(&self, capabilities: &mut Value) {
        if self.suppress_pull_diagnostics
            && let Some(text_document) = capabilities
                .get_mut("textDocument")
                .and_then(Value::as_object_mut)
        {
            text_document.remove("diagnostic");
        }
    }

    /// Resolves the effective `initializationOptions` for the server's initialize
    /// — the init-options-assembly seam.
    ///
    /// The profile's forced conformance options are overlaid onto the
    /// `user`-supplied options and **win on conflict**: they are applied after the
    /// user/project merge and are not overridable (existing [`deep_merge`]
    /// semantics — the forced options are the overlay). A user's *unrelated* keys
    /// survive; a user value for a forced key is replaced. With no forced options
    /// this is the identity on `user`.
    #[must_use]
    pub fn effective_initialization_options(&self, user: Option<&Value>) -> Option<Value> {
        match (self.forced_initialization_options.as_ref(), user) {
            (Some(forced), Some(user)) => Some(deep_merge(user, forced)),
            (Some(forced), None) => Some(forced.clone()),
            (None, user) => user.cloned(),
        }
    }
}

/// The engine-internal per-server profile table — the single source of casing
/// knowledge. Every entry is a conformance invariant the CI conformance matrix
/// re-verifies per re-pin. A server absent from the table gets the default
/// (empty) profile.
fn profile(server_name: &str) -> ServerProfile {
    match server_name {
        "rust-analyzer" => ServerProfile {
            suppress_pull_diagnostics: true,
            forced_initialization_options: None,
        },
        "gopls" => ServerProfile {
            suppress_pull_diagnostics: false,
            forced_initialization_options: Some(json!({
                "pullDiagnostics": true,
                "diagnosticsDelay": "0s",
            })),
        },
        _ => ServerProfile::default(),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::ServerProfile;
    use serde_json::json;

    #[test]
    fn rust_analyzer_suppresses_pull() {
        assert!(ServerProfile::for_server("rust-analyzer").suppresses_pull_diagnostics());
    }

    #[test]
    fn other_servers_do_not_suppress_pull() {
        for name in ["gopls", "lattice", "clangd", "typescript-ls", "yX4Za"] {
            assert!(
                !ServerProfile::for_server(name).suppresses_pull_diagnostics(),
                "{name} must not suppress pull",
            );
        }
    }

    #[test]
    fn shape_removes_diagnostic_for_suppressed_server() {
        let mut caps = json!({
            "textDocument": { "diagnostic": { "dynamicRegistration": false }, "definition": {} }
        });
        ServerProfile::for_server("rust-analyzer").shape_client_capabilities(&mut caps);
        assert!(caps["textDocument"].get("diagnostic").is_none());
        // Only `diagnostic` is dropped; siblings survive.
        assert!(caps["textDocument"].get("definition").is_some());
    }

    #[test]
    fn shape_is_identity_for_uncased_server() {
        let original = json!({
            "textDocument": { "diagnostic": { "dynamicRegistration": false } }
        });
        let mut caps = original.clone();
        ServerProfile::for_server("clangd").shape_client_capabilities(&mut caps);
        assert_eq!(caps, original);
    }

    #[test]
    fn gopls_forces_both_levers_when_user_supplies_none() {
        let opts = ServerProfile::for_server("gopls")
            .effective_initialization_options(None)
            .expect("gopls forces initialization options");
        assert_eq!(opts["pullDiagnostics"], json!(true));
        assert_eq!(opts["diagnosticsDelay"], json!("0s"));
    }

    #[test]
    fn gopls_conformance_wins_over_user_options() {
        // A user tries to override a conformance lever and adds an unrelated key.
        let user = json!({ "diagnosticsDelay": "250ms", "buildFlags": ["-tags=integration"] });
        let opts = ServerProfile::for_server("gopls")
            .effective_initialization_options(Some(&user))
            .expect("merged options");
        // Conformance wins on the conflicting key — never overridable.
        assert_eq!(opts["diagnosticsDelay"], json!("0s"));
        assert_eq!(opts["pullDiagnostics"], json!(true));
        // The user's unrelated key survives.
        assert_eq!(opts["buildFlags"], json!(["-tags=integration"]));
    }

    #[test]
    fn uncased_server_passes_user_options_through() {
        let user = json!({ "some": "option" });
        assert_eq!(
            ServerProfile::for_server("clangd").effective_initialization_options(Some(&user)),
            Some(user.clone()),
        );
        assert_eq!(
            ServerProfile::for_server("clangd").effective_initialization_options(None),
            None,
        );
    }
}
