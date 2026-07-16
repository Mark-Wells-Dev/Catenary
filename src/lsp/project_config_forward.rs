// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The project-config forwarding transport (misc 202 follow-up).
//!
//! A language server's per-project lint/feature surface is a **project
//! property**: rust-analyzer reads `check.command`, `cargo.features`, and the
//! like from a `rust-analyzer.toml` at the project root (misc 202). But
//! rust-analyzer's *workspace-level* config-file support is documented
//! work-in-progress — the installed binary ignores those keys read from a
//! project-level file, so the E0432 false reds persist and flycheck still runs
//! plain `cargo check` (verified live on a fresh daemon, 2026-07-16). The keys
//! are honored only from **client** (or user-level) config.
//!
//! So Catenary is a **dumb transport**: it reads the project's config file at
//! spawn and forwards its contents through the LSP client-config channel — the
//! `initializationOptions` seam. The file stays the user's single editable
//! source of truth; Catenary owns no settings, coins no keys, and never rewrites
//! the file. This also completes the conformance story: settings a server reads
//! straight from a file would **evade** Catenary's conformance-critical overrides
//! (the forced / forbidden `initializationOptions` in
//! [`crate::lsp::server_behavior::ServerProfile::effective_initialization_options`]),
//! so routing the file's data through the client channel — where the forced
//! overlay still wins — is required regardless.
//!
//! # What this module owns
//!
//! - [`translate_toml`]: the pure translation. A server's config file mirrors its
//!   client-config namespace one-to-one (rust-analyzer.toml keys are the
//!   `rust-analyzer.*` settings without the prefix), so the mapping is the
//!   generic TOML → JSON value map plus TOML's own dotted-key / sub-table
//!   nesting — no per-key hand-coding. `check.command = "clippy"` becomes
//!   `{"check": {"command": "clippy"}}`; `cargo.features = ["mockls"]` becomes
//!   `{"cargo": {"features": ["mockls"]}}`.
//! - [`forwarded_options`]: the transport. Given a served root and a resolved
//!   [`ServerProfile`], it reads the profile's project-config file (if the
//!   profile carries a convention — [`ServerProfile::project_config`]) from that
//!   root and returns the translated JSON, or `None`.
//!
//! # Failure honesty
//!
//! A bad config file never blocks a spawn. An **absent** file yields `None` (the
//! pre-misc-202 behavior — no forwarded options). An **unreadable or invalid**
//! file emits a `warn!` (a TUI finding, no interrupt) naming the file and the
//! parse/IO error, then yields `None` — the spawn proceeds with no forwarded
//! options rather than failing.
//!
//! # Scope
//!
//! Spawn-time only: the file is read once, as the server is spawned for a root.
//! A mid-session edit to the file takes effect on the server's **next** spawn.
//! Pushing a live update would use `workspace/didChangeConfiguration` (RA
//! supports it); that is a deliberate future improvement, not built here.

use std::path::Path;

use serde_json::Value;
use tracing::warn;

use crate::lsp::server_behavior::ServerProfile;
use crate::source::Source;

/// The forwarded client-config options for a server spawned at `root`, or `None`.
///
/// The transport leg. When `profile` carries a project-config convention
/// ([`ServerProfile::project_config`]), the convention's file is looked up at
/// `root` and, if present and valid, translated ([`translate_toml`]) into the
/// client-config JSON the server reads. Returns `None` when:
///
/// - the profile carries **no** convention (the common case — nothing to
///   forward), or
/// - the convention's file is **absent** at `root` (pre-misc-202 behavior), or
/// - the file is **unreadable or invalid TOML** — a `warn!` names the file and
///   the error, and the spawn proceeds with no forwarded options (a bad config
///   file never blocks a spawn).
///
/// The returned value is the **project's data**; the caller layers it *under*
/// the user's machine-level Catenary-config server options and then through
/// [`ServerProfile::effective_initialization_options`], so the conformance
/// forced overlay still wins over both (see [`crate::lsp::manager`]).
#[must_use]
pub fn forwarded_options(root: &Path, profile: &ServerProfile) -> Option<Value> {
    let convention = profile.project_config()?;
    let path = root.join(&convention.file);

    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Absent file: the pre-misc-202 behavior — no forwarded options,
            // silently. The SessionStart nudge (crate::lsp::project_config) is
            // the surface that points the agent at the missing file.
            return None;
        }
        Err(e) => {
            // Present but unreadable (permissions, a directory, …): warn and
            // proceed with nothing forwarded, never blocking the spawn.
            warn!(
                source = Source::LspLifecycle.as_str(),
                file = %path.display(),
                error = %e,
                "project config unreadable — spawning with no forwarded options",
            );
            return None;
        }
    };

    match translate_toml(&contents) {
        Ok(value) => Some(value),
        Err(e) => {
            warn!(
                source = Source::LspLifecycle.as_str(),
                file = %path.display(),
                error = %e,
                "project config is invalid TOML — spawning with no forwarded options",
            );
            None
        }
    }
}

/// Translate a server config file's TOML source into the client-config JSON the
/// server reads.
///
/// The mapping is generic, never per-key: a server's config-file keys mirror its
/// client-config namespace, so translation is the TOML → JSON value map plus
/// TOML's native nesting. TOML's own parser resolves dotted keys and `[table]`
/// headers into nested tables before we ever see them, so `check.command` and
/// `[check]\ncommand = …` both arrive as a nested `check` table and both render
/// `{"check": {"command": …}}` — the dotted-key nesting is TOML's job, not ours.
///
/// # Errors
///
/// Returns the [`toml::de::Error`] when `contents` is not valid TOML. An **empty**
/// source is valid TOML (the empty table) and yields the empty JSON object.
pub fn translate_toml(contents: &str) -> Result<Value, toml::de::Error> {
    let table: toml::Table = contents.parse()?;
    Ok(toml_to_json(&toml::Value::Table(table)))
}

/// Convert a [`toml::Value`] into the equivalent [`serde_json::Value`].
///
/// Tables become objects, arrays become arrays, scalars map across the two value
/// spaces. TOML datetimes stringify (JSON has no datetime scalar); a TOML integer
/// is always `i64`, which is representable in a JSON number, so the mapping is
/// total for any parsed TOML value.
///
/// This mirrors the private converter in
/// [`crate::lsp::server_behavior`] (which maps the manifest's forced-init-options
/// the same way); the two are kept separate so this transport carries no
/// dependency on the conformance seam's internals.
fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(Value::Null, Value::Number)
        }
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(dt) => Value::String(dt.to_string()),
        toml::Value::Array(arr) => Value::Array(arr.iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    // ── translate_toml: the pure translation ────────────────────────────

    #[test]
    fn translates_a_dotted_key_to_a_nested_object() {
        // The motivating case: `check.command = "clippy"` nests under `check`.
        let value = translate_toml("check.command = \"clippy\"\n").expect("valid TOML");
        assert_eq!(value, json!({ "check": { "command": "clippy" } }));
    }

    #[test]
    fn translates_an_array_value() {
        // `cargo.features = ["mockls"]` — arrays map straight across.
        let value = translate_toml("cargo.features = [\"mockls\"]\n").expect("valid TOML");
        assert_eq!(value, json!({ "cargo": { "features": ["mockls"] } }));
    }

    #[test]
    fn table_headers_and_dotted_keys_translate_identically() {
        // TOML resolves `[check]\ncommand=…` and `check.command=…` to the same
        // nested table before we translate, so both render the same JSON — the
        // dotted-key nesting is TOML's job.
        let dotted = translate_toml("check.command = \"clippy\"\ncheck.features = [\"a\"]\n")
            .expect("valid dotted TOML");
        let header = translate_toml("[check]\ncommand = \"clippy\"\nfeatures = [\"a\"]\n")
            .expect("valid header TOML");
        assert_eq!(dotted, header);
        assert_eq!(
            dotted,
            json!({ "check": { "command": "clippy", "features": ["a"] } }),
        );
    }

    #[test]
    fn translates_the_full_misc_202_config() {
        // The exact file the ticket describes: clippy flycheck + a build feature
        // on both the cargo and check axes.
        let source = "\
check.command = \"clippy\"
cargo.features = [\"mockls\"]
check.features = [\"mockls\"]
";
        let value = translate_toml(source).expect("valid TOML");
        assert_eq!(
            value,
            json!({
                "check": { "command": "clippy", "features": ["mockls"] },
                "cargo": { "features": ["mockls"] },
            }),
        );
    }

    #[test]
    fn translates_scalars_across_the_value_spaces() {
        let source = "\
s = \"str\"
i = 42
f = 1.5
b = true
";
        let value = translate_toml(source).expect("valid TOML");
        assert_eq!(value["s"], json!("str"));
        assert_eq!(value["i"], json!(42));
        assert_eq!(value["f"], json!(1.5));
        assert_eq!(value["b"], json!(true));
    }

    #[test]
    fn an_empty_file_is_the_empty_object() {
        // Empty is valid TOML (the empty table) — the empty object, never an
        // error and never null.
        let value = translate_toml("").expect("empty is valid TOML");
        assert_eq!(value, json!({}));
        // Whitespace / a comment only is still empty.
        let value = translate_toml("# just a comment\n\n").expect("comment-only is valid TOML");
        assert_eq!(value, json!({}));
    }

    #[test]
    fn invalid_toml_is_an_error() {
        // A bare unparseable line is not TOML — translation surfaces the error
        // (the transport turns this into a warn!, never a panic).
        assert!(
            translate_toml("this is not toml =").is_err(),
            "a malformed assignment must not translate",
        );
        assert!(
            translate_toml("[unterminated").is_err(),
            "an unterminated table header must not translate",
        );
    }

    // ── forwarded_options: the transport ────────────────────────────────

    #[test]
    fn forwards_the_translated_file_for_a_convention_carrying_profile() {
        // rust-analyzer carries the `rust-analyzer.toml` convention; a present,
        // valid file at the root is read and translated.
        let root = TempDir::new().expect("tempdir");
        fs::write(
            root.path().join("rust-analyzer.toml"),
            "check.command = \"clippy\"\ncargo.features = [\"mockls\"]\n",
        )
        .expect("write rust-analyzer.toml");

        let profile = ServerProfile::for_server("rust-analyzer");
        let options = forwarded_options(root.path(), &profile)
            .expect("a present valid file yields forwarded options");
        assert_eq!(
            options,
            json!({
                "check": { "command": "clippy" },
                "cargo": { "features": ["mockls"] },
            }),
        );
    }

    #[test]
    fn absent_file_forwards_nothing() {
        // A Rust root with no rust-analyzer.toml: the pre-misc-202 behavior —
        // no forwarded options, no warning.
        let root = TempDir::new().expect("tempdir");
        let profile = ServerProfile::for_server("rust-analyzer");
        assert_eq!(
            forwarded_options(root.path(), &profile),
            None,
            "an absent convention file forwards nothing",
        );
    }

    #[test]
    fn a_profile_without_a_convention_forwards_nothing() {
        // gopls carries no project-config convention, so even a file named
        // like a config at the root is never read — the transport is
        // convention-driven, not "any file at the root".
        let root = TempDir::new().expect("tempdir");
        fs::write(
            root.path().join("rust-analyzer.toml"),
            "check.command = \"clippy\"\n",
        )
        .expect("write a file");
        let profile = ServerProfile::for_server("gopls");
        assert_eq!(
            forwarded_options(root.path(), &profile),
            None,
            "a server with no convention forwards nothing",
        );
    }

    #[test]
    fn invalid_file_forwards_nothing_without_blocking() {
        // A present but malformed file: the transport returns None (a warn! is
        // emitted) rather than erroring — the spawn is never blocked on a bad
        // config file.
        let root = TempDir::new().expect("tempdir");
        fs::write(
            root.path().join("rust-analyzer.toml"),
            "this is not = valid = toml =",
        )
        .expect("write bad rust-analyzer.toml");
        let profile = ServerProfile::for_server("rust-analyzer");
        assert_eq!(
            forwarded_options(root.path(), &profile),
            None,
            "an invalid config file forwards nothing (and does not panic)",
        );
    }

    #[test]
    fn an_empty_file_forwards_the_empty_object() {
        // An empty (but present) file is valid — it forwards the empty object,
        // distinct from an absent file's None. (Layered under user options, the
        // empty object is inert; the distinction is honest about "present but
        // says nothing" vs "not there".)
        let root = TempDir::new().expect("tempdir");
        fs::write(root.path().join("rust-analyzer.toml"), "\n").expect("write empty file");
        let profile = ServerProfile::for_server("rust-analyzer");
        assert_eq!(
            forwarded_options(root.path(), &profile),
            Some(json!({})),
            "a present empty file forwards the empty object",
        );
    }
}
