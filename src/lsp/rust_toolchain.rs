// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Spawn-time rust-toolchain pin resolution (misc 176 / bug 92).
//!
//! On layouts where rustup proxies are absent (Homebrew rustup out of the box)
//! or bypassed, the `rust-analyzer` Catenary spawns — and, critically, the
//! `cargo`/`rustc` that rust-analyzer spawns for flycheck — ignore the project's
//! `rust-toolchain.toml` pin. Diagnostics then come from the wrong compiler:
//! receipts quietly wrong (the ghost smoke: repo pins 1.95, unproxied
//! invocations ran 1.97).
//!
//! The fix is layout-independent and rust-engine-cased. At server spawn, per
//! scope root, [`resolve_active_toolchain`] asks the `rustup` binary itself —
//! which exists even when the proxies do not — to name the active toolchain for
//! that root, delegating *all* resolution semantics (`rust-toolchain.toml`,
//! legacy `rust-toolchain`, directory overrides, the default) to rustup. When a
//! toolchain resolves, [`wrap_spawn`] rewrites the spawn to run through
//! `rustup run <toolchain> …` and sets `RUSTUP_TOOLCHAIN` in the child env, so
//! both the server component AND the flycheck `cargo`/`rustc` it spawns match
//! the pin on every layout.
//!
//! ## Mechanism: `rustup run`, not a bare PATH-prepend
//!
//! `rustup run <toolchain> <cmd>` prepends the resolved toolchain's bin dir to
//! the child's PATH *and* sets `RUSTUP_TOOLCHAIN`, which is exactly what makes
//! the pin reach the flycheck sub-invocations rust-analyzer itself spawns — a
//! bare PATH-prepend of the toolchain bin dir would fix the server binary but
//! not reliably the sub-processes that re-resolve through proxies. It also fits
//! the existing `program`/`args` plumbing with no new resolution step: we only
//! rewrite `program` to `rustup` and prepend `["run", <toolchain>, <program>]`
//! to the args. `RUSTUP_TOOLCHAIN` is set too as belt-and-braces: it covers any
//! proxied sub-invocation that bypasses `rustup run`'s PATH setup.
//!
//! ## Precedence: an operator's explicit binary still wins
//!
//! Wrapping applies only when the server is the rust engine (`rust-analyzer`)
//! *and* the configured program is the bare `rust-analyzer` key — i.e. the
//! rustup proxy resolved on PATH (misc 162: "the server key IS the executable").
//! A `[lsp.server.rust-analyzer] path = "…"` override, or any configured program
//! that is not the bare key, is explicit operator intent to spawn a concrete
//! binary; we do not second-guess it. See [`should_wrap`].
//!
//! ## Lifetime
//!
//! Resolution runs at spawn (servers are per-root instances) — never per
//! request. Nothing is cached across spawns; a re-spawn re-resolves and so picks
//! up toolchain-file edits.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use tracing::debug;

/// The server key of the rust engine — the sole engine this casing applies to.
const RUST_ANALYZER_KEY: &str = "rust-analyzer";

/// A spawn rewritten to run through the resolved rustup toolchain.
///
/// Produced by [`wrap_spawn`]. `program`/`args` replace the original spawn
/// command; `env` carries the extra child-environment entries to overlay
/// (`RUSTUP_TOOLCHAIN`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolchainSpawn {
    /// The program to spawn — always `rustup`.
    pub program: String,
    /// The full argument vector: `["run", <toolchain>, <original program>, <original args…>]`.
    pub args: Vec<String>,
    /// Extra environment entries to overlay on the child (belt-and-braces
    /// `RUSTUP_TOOLCHAIN=<toolchain>`, covering proxied sub-invocations).
    pub env: HashMap<String, String>,
    /// The resolved toolchain name (for tracing / callers).
    pub toolchain: String,
}

/// Whether the rust-toolchain casing applies to this server + program pair.
///
/// True only for the rust engine spawned through the bare `rust-analyzer` key —
/// i.e. the rustup proxy resolved on PATH (misc 162). A `path` override (any
/// `program` other than the bare key) is explicit operator intent to spawn a
/// concrete binary and is left untouched: the operator's absolute path wins.
#[must_use]
pub fn should_wrap(server_name: &str, program: &str) -> bool {
    server_name == RUST_ANALYZER_KEY && program == RUST_ANALYZER_KEY
}

/// Resolve the active toolchain for `root` by invoking `rustup` itself.
///
/// Runs `rustup show active-toolchain` with `cwd = root`, delegating all
/// resolution semantics (`rust-toolchain.toml`, legacy `rust-toolchain`,
/// directory overrides, the default) to rustup. Returns the parsed toolchain
/// name on success, or `None` when:
///
/// - `rustup` is not on PATH (the command fails to spawn),
/// - the command exits non-zero (no toolchain resolves for the root),
/// - the output does not parse to a toolchain name.
///
/// Each `None` path traces *why* at `debug!` — a resolution outcome, not an
/// actionable break, so it never earns `warn!`/`error!`.
#[must_use]
pub fn resolve_active_toolchain(root: &Path) -> Option<String> {
    let output = match Command::new("rustup")
        .args(["show", "active-toolchain"])
        .current_dir(root)
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            debug!(
                scope_root = %root.display(),
                "rust-toolchain: `rustup` not runnable ({e}) — spawning rust-analyzer unchanged",
            );
            return None;
        }
    };

    if !output.status.success() {
        // No toolchain resolves for this root (e.g. no default, no pin). Not an
        // error — the caller spawns unchanged and any real pin problem surfaces
        // at the spawn-failure boundary if it exists.
        let stderr = String::from_utf8_lossy(&output.stderr);
        debug!(
            scope_root = %root.display(),
            status = %output.status,
            stderr = %stderr.trim(),
            "rust-toolchain: `rustup show active-toolchain` failed — spawning rust-analyzer unchanged",
        );
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some(toolchain) = parse_active_toolchain(&stdout) else {
        debug!(
            scope_root = %root.display(),
            raw = %stdout.trim(),
            "rust-toolchain: could not parse `rustup show active-toolchain` output — \
             spawning rust-analyzer unchanged",
        );
        return None;
    };
    debug!(
        scope_root = %root.display(),
        toolchain = %toolchain,
        "rust-toolchain: resolved active toolchain",
    );
    Some(toolchain)
}

/// Parse the toolchain name from `rustup show active-toolchain` output.
///
/// Defensive across rustup output shapes surveyed for this fix:
///
/// - `1.95-x86_64-unknown-linux-gnu (overridden by '/…/rust-toolchain.toml')`
/// - `stable-x86_64-unknown-linux-gnu (default)`
/// - `1.95-aarch64-apple-darwin (directory override for '/…')`
/// - `nightly-2025-01-01-x86_64-unknown-linux-gnu` (bare name, no annotation)
///
/// The name is always the first whitespace-delimited token of the first
/// non-empty line; the `(…)` source annotation, when present, always follows
/// whitespace. Returns `None` for empty output or an `error…` line (some rustup
/// versions print resolution errors to stdout).
#[must_use]
pub fn parse_active_toolchain(raw: &str) -> Option<String> {
    let first = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    // Some rustup versions emit `error: …` on stdout for an unresolved root.
    if first
        .get(..6)
        .is_some_and(|p| p.eq_ignore_ascii_case("error:"))
    {
        return None;
    }
    let name = first.split_whitespace().next()?;
    if name.is_empty() {
        return None;
    }
    Some(name.to_string())
}

/// Rewrite a spawn to run through `rustup run <toolchain> …`.
///
/// The result replaces the original `program`/`args`:
///
/// ```text
/// program = "rustup"
/// args    = ["run", <toolchain>, <program>, <original args…>]
/// env     += RUSTUP_TOOLCHAIN=<toolchain>
/// ```
///
/// Callers gate on [`should_wrap`] and resolve `toolchain` via
/// [`resolve_active_toolchain`] before calling this. The returned `env` is an
/// *overlay* — the caller merges it onto the configured server env (with the
/// config value winning on key conflict, matching `ServerDef::env` semantics).
#[must_use]
pub fn wrap_spawn(program: &str, args: &[&str], toolchain: &str) -> ToolchainSpawn {
    let mut wrapped_args = Vec::with_capacity(args.len() + 3);
    wrapped_args.push("run".to_string());
    wrapped_args.push(toolchain.to_string());
    wrapped_args.push(program.to_string());
    wrapped_args.extend(args.iter().map(|a| (*a).to_string()));

    let mut env = HashMap::new();
    env.insert("RUSTUP_TOOLCHAIN".to_string(), toolchain.to_string());

    ToolchainSpawn {
        program: "rustup".to_string(),
        args: wrapped_args,
        env,
        toolchain: toolchain.to_string(),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::{parse_active_toolchain, should_wrap, wrap_spawn};

    // ── parse_active_toolchain: output-shape coverage ──────────────────

    #[test]
    fn parses_override_by_toolchain_file() {
        // rustup 1.2x with a rust-toolchain.toml pin.
        let raw = "1.95-x86_64-unknown-linux-gnu (overridden by \
                   '/home/u/proj/rust-toolchain.toml')\n";
        assert_eq!(
            parse_active_toolchain(raw).as_deref(),
            Some("1.95-x86_64-unknown-linux-gnu"),
        );
    }

    #[test]
    fn parses_default_annotation() {
        let raw = "stable-x86_64-unknown-linux-gnu (default)\n";
        assert_eq!(
            parse_active_toolchain(raw).as_deref(),
            Some("stable-x86_64-unknown-linux-gnu"),
        );
    }

    #[test]
    fn parses_directory_override() {
        let raw = "1.95-aarch64-apple-darwin (directory override for '/Users/u/proj')\n";
        assert_eq!(
            parse_active_toolchain(raw).as_deref(),
            Some("1.95-aarch64-apple-darwin"),
        );
    }

    #[test]
    fn parses_bare_name_without_annotation() {
        // A bare toolchain name with no parenthetical source (some versions).
        let raw = "nightly-2025-01-01-x86_64-unknown-linux-gnu\n";
        assert_eq!(
            parse_active_toolchain(raw).as_deref(),
            Some("nightly-2025-01-01-x86_64-unknown-linux-gnu"),
        );
    }

    #[test]
    fn parses_with_leading_and_trailing_whitespace() {
        let raw = "\n   1.95-x86_64-unknown-linux-gnu (default)   \n";
        assert_eq!(
            parse_active_toolchain(raw).as_deref(),
            Some("1.95-x86_64-unknown-linux-gnu"),
        );
    }

    #[test]
    fn takes_only_the_first_line() {
        // A second line (e.g. a version banner some shapes append) is ignored.
        let raw = "1.95-x86_64-unknown-linux-gnu (default)\nactive because: it is the default\n";
        assert_eq!(
            parse_active_toolchain(raw).as_deref(),
            Some("1.95-x86_64-unknown-linux-gnu"),
        );
    }

    #[test]
    fn rejects_empty_output() {
        assert_eq!(parse_active_toolchain(""), None);
        assert_eq!(parse_active_toolchain("\n\n   \n"), None);
    }

    #[test]
    fn rejects_error_line_on_stdout() {
        // Some rustup versions print the resolution error to stdout.
        let raw = "error: no override and no default toolchain set\n";
        assert_eq!(parse_active_toolchain(raw), None);
        // Case-insensitive on the marker.
        assert_eq!(parse_active_toolchain("Error: nope\n"), None);
    }

    // ── should_wrap: the engine-casing + precedence predicate ──────────

    #[test]
    fn wraps_only_the_bare_rust_analyzer_key() {
        assert!(should_wrap("rust-analyzer", "rust-analyzer"));
    }

    #[test]
    fn does_not_wrap_a_path_override() {
        // `[lsp.server.rust-analyzer] path = "/opt/ra/rust-analyzer"` → explicit
        // operator intent; the absolute program wins, untouched.
        assert!(!should_wrap("rust-analyzer", "/opt/ra/bin/rust-analyzer"));
    }

    #[test]
    fn does_not_wrap_other_engines() {
        for (server, program) in [
            ("gopls", "gopls"),
            ("clangd", "clangd"),
            ("lattice", "lattice"),
            ("rust-analyzer-nightly", "rust-analyzer-nightly"),
        ] {
            assert!(
                !should_wrap(server, program),
                "{server}/{program} must not be wrapped",
            );
        }
    }

    // ── wrap_spawn: command/env construction ───────────────────────────

    #[test]
    fn wrap_builds_rustup_run_invocation() {
        let wrapped = wrap_spawn("rust-analyzer", &[], "1.95-x86_64-unknown-linux-gnu");
        assert_eq!(wrapped.program, "rustup");
        assert_eq!(
            wrapped.args,
            vec!["run", "1.95-x86_64-unknown-linux-gnu", "rust-analyzer"],
        );
        assert_eq!(
            wrapped.env.get("RUSTUP_TOOLCHAIN").map(String::as_str),
            Some("1.95-x86_64-unknown-linux-gnu"),
        );
        assert_eq!(wrapped.toolchain, "1.95-x86_64-unknown-linux-gnu");
    }

    #[test]
    fn wrap_preserves_original_args_in_order() {
        let wrapped = wrap_spawn("rust-analyzer", &["--log-file", "/tmp/ra.log"], "stable");
        assert_eq!(
            wrapped.args,
            vec![
                "run",
                "stable",
                "rust-analyzer",
                "--log-file",
                "/tmp/ra.log"
            ],
        );
    }
}
