// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Low-level LSP client for communicating with a server process.
pub mod client;
/// Transport layer: process lifecycle, reader loop, request/response correlation.
pub(crate) mod connection;
/// Extractor functions for LSP response and notification fields.
pub(crate) mod extract;
/// LSP file watcher glob patterns and change types.
pub mod glob;
/// Instance identity types for LSP server routing.
pub mod instance_key;
/// Standalone pure functions for LSP document identity.
pub mod lang;
/// High-level manager for lazy-spawning and caching LSP clients.
pub mod manager;
/// Builder functions for LSP request and notification parameters.
pub(crate) mod params;
/// The `SessionStart` project-config setup nudge (misc 202).
pub mod project_config;
/// The project-config forwarding transport (misc 202 follow-up): read the
/// project's server config file and forward it as client config.
pub mod project_config_forward;
/// LSP message protocol definitions.
pub mod protocol;
/// Spawn-time rust-toolchain pin resolution (misc 176 / bug 92).
pub mod rust_toolchain;
/// Server profile: init-time capabilities and runtime observations.
pub(crate) mod server;
/// Engine-internal per-server behavior casing (capability shaping, shipped levers).
pub mod server_behavior;
/// Idle detection and profiling: polls process tree for quiet detection.
pub mod settle;
/// Server state and progress tracking.
pub mod state;
/// Small local types for LSP concepts.
pub(crate) mod types;

/// Shared test helpers for LSP unit tests.
///
/// Gated on `feature = "mockls"` so in-crate unit tests and integration
/// tests share the same helpers.
#[cfg(feature = "mockls")]
#[doc(hidden)]
#[allow(
    clippy::expect_used,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    missing_docs,
    reason = "test-only module, doc lints are noise"
)]
pub mod test_support {
    use std::path::PathBuf;

    /// Locates the mockls binary relative to the test binary.
    ///
    /// Test binaries live in `target/debug/deps/`; mockls lives in
    /// `target/debug/`. Navigate up from the test binary to find it.
    pub fn mockls_bin() -> PathBuf {
        let mut path = std::env::current_exe().expect("current_exe");
        path.pop(); // binary name → deps/
        path.pop(); // deps/ → debug/
        path.push("mockls");
        assert!(
            path.exists(),
            "mockls not found at {} — build with --features mockls",
            path.display()
        );
        path
    }
}

pub use client::{DocSync, LspClient};
pub use instance_key::{InstanceKey, Scope};
pub use manager::LspClientManager;
pub(crate) use manager::WalkBreadth;
pub use server::LspServer;
pub use state::{ProgressTracker, ServerLifecycle, ServerStatus};
