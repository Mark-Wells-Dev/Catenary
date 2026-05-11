# Catenary Agent Context

This file serves as the single point of truth for AI agents (Claude, Gemini, etc.) working on the Catenary project.

## Project Grounding

- **Project Goal:** High-performance multiplexing bridge between MCP and LSP.
- **Repository:** `TwoWells/Catenary` on GitHub.
- **Config:** `@./Cargo.toml`
- **Dependency Policy:** `@./deny.toml`
- **Documentation:** `docs/src/`

## How Catenary Works

Catenary is an MCP server that gives AI agents LSP-powered code intelligence. It
multiplexes one or more LSP servers (e.g., rust-analyzer, typescript-language-server)
behind a single MCP interface, providing hover, go-to-definition, diagnostics,
find-references, rename, and search without shell-based text scanning.

### Core concepts

- **Session:** A running Catenary instance. Each session has a unique ID (opaque
  string), a PID, and one or more workspace roots. Sessions are discoverable via
  `catenary list` and monitorable via `catenary monitor <id>`. See `src/session.rs`.
- **Database:** All session state (sessions, events, workspace roots) is stored in
  `~/.local/state/catenary/catenary.db` (SQLite with WAL mode). See `src/db.rs`.
- **MCP tools:** The tools exposed to agents (search, hover, definition,
  diagnostics, etc.) are defined in the MCP server. Each tool delegates to one or
  more LSP servers under the hood.
- **Hooks:** Catenary integrates with host CLIs (Claude Code, Gemini CLI) via
  hooks that fire before/after tool use. Hook definitions live in
  `plugins/catenary/hooks/hooks.json` (Claude Code) and `hooks/hooks.json`
  (Gemini CLI).
- **Diagnostics:** The `catenary hook post-tool` command (`src/hook.rs` for the
  IPC server) runs in PostToolUse hooks after file edits. It connects to the
  running session's hook socket, sends the changed file path, and returns
  LSP diagnostics so they appear in the model's context. Diagnostic events are
  stored in the SQLite database for later querying via `catenary query`.
- **Root sync:** `catenary hook pre-tool` (PreToolUse, Claude Code only) scans
  the transcript for `/add-dir` workspace additions and forwards them to the session.
- **Logging:** `LoggingServer` is a `tracing_subscriber::Layer` that subscribes
  to every tracing event and dispatches to multiple sinks: notification queue
  (user-facing `systemMessage`), protocol DB (LSP/MCP/hook messages), and trace
  DB (non-protocol events). See `src/logging/mod.rs` and
  `docs/src/tracing-conventions.md`.

### Key source files

- `src/db.rs` — SQLite connection management, schema creation, and migrations.
- `src/logging/mod.rs` — `LoggingServer`: multi-sink tracing Layer, the sole
  telemetry port/adapter. Dispatches to notification queue, protocol DB, and
  trace DB sinks.
- `src/session.rs` — session lifecycle and event broadcasting.
- `docs/src/` — full documentation source.

## Coding Standards

- **Edition:** Rust 2024.
- **Safety:** `unsafe` code is strictly forbidden (`forbid(unsafe_code)`).
- **Error Handling:** Use `anyhow` for application logic and `thiserror` for library errors.
- **Strict Denials:** Do NOT use `unwrap()`, `panic!()`, `todo!()`, `unimplemented!()`, `dbg!()`, `println!()`, or `eprintln!()`. Use proper error handling and the `tracing` crate for logging. `expect()` is denied in production code but allowed in `#[cfg(test)]` modules — prefer `expect("reason")` over `anyhow` workarounds in tests.
- **Tracing:** `warn!()` and `error!()` events reach the user-notification queue by default. Only use these levels for user-relevant, actionable conditions. Internal diagnostics belong at `info!()` or `debug!()`. See `docs/src/tracing-conventions.md` for severity guidelines, reserved structured fields, and the `source` taxonomy.
- **Imports:** No wildcard imports (`use crate::*`).
- **Formatting:** Code must be formatted with `rustfmt`.
- **Linting:** Must pass `cargo clippy` with `pedantic`, `nursery`, and `cargo` groups enabled.
- **Dependencies:** Must pass `cargo machete` (no unused dependencies).

## Quality Standards

- **License Compliance:** All new dependencies MUST have permissive licenses (MIT, Apache-2.0, etc.) as specified in `@./deny.toml`. Catenary is dual-licensed under AGPL-3.0-or-later and a commercial license.
- **Documentation:** All public APIs must have documentation comments.
- **Testing:**
  - All new features must include tests.
  - Integration tests in `tests/` often require real LSP servers (e.g., `rust-analyzer`).
  - Integration test subprocesses (bridge, `catenary install`, etc.) must call `isolate_env(&mut cmd, root)` **before** setting any `CATENARY_*` env vars. `isolate_env` clears all inherited `CATENARY_*` and `XDG_*` vars, then sets `XDG_CONFIG_HOME`, `XDG_STATE_HOME`, and `XDG_DATA_HOME` to the test's tempdir. Callers then set `CATENARY_SERVERS`, `CATENARY_ROOTS`, or `CATENARY_CONFIG` after the call — these overwrite the cleared values. Without `isolate_env`, subprocesses inherit the user's shell environment, writing to `~/.config`, `~/.local/state`, or `~/.local/share` and causing races between parallel tests and across worktrees.

## Development Commands

- **Check (full):** `make check` — format, lint, deny, machete, and test in one pass.
- **Test (all):** `make test`
- **Test (filtered):** `make test T=<filter>` — run only tests matching the filter (e.g., `make test T=json_diagnostics`).
- **Test (repeat):** `make test T=<filter> N=<count>` — stress-test by repeating N times (e.g., `make test T=flaky_test N=5`).
- **Mutation testing:** `make mutants` — pre-release only. Pass `T=<module>` to scope (e.g., `make mutants T=command_filter`).
- **Stop mutation testing:** `make mutants-stop` — kills cargo-mutants and all child processes. **Never use `pkill cargo-mutants`** — it kills only the parent, orphaning test binaries that run without timeout or memory limits. An orphaned mutant test binary caused a 41.8GB OOM that crashed the GPU driver.

## Release Workflow

Versioning and releases are managed via the `Makefile`.

- **Patch Release:** `make release-patch` (e.g., 0.1.0 -> 0.1.1)
- **Minor Release:** `make release-minor` (e.g., 0.1.0 -> 0.2.0)
- **Major Release:** `make release-major` (e.g., 0.1.0 -> 1.0.0)
- **Custom Version:** `make release V=x.y.z`

These commands automatically:

1. Verify the working tree is clean and on `main`.
2. Run `cargo update` to ensure `Cargo.lock` is fresh.
3. Bump versions in `Cargo.toml` and `.claude-plugin/marketplace.json`.
4. Run all tests and linting checks.
5. Commit the changes and create a git tag.

To complete the release, push the changes and tags:
`git push && git push --tags`

Pushing the tag triggers the CD workflow (binary builds + crates.io
publish) and a docs rebuild. The docs workflow builds stable docs
from the latest `v*` tag, so any docs changes on `main` only reach
`/stable/` after a tagged release. Dev docs at `/dev/` update on
every push to `main`.

### Pre-release checklist

Before running `make release-*`:

1. Ensure `git push` has been run so local `main` matches `origin/main`.
2. Run `make mutants` and address any surviving mutants (for major releases).

If checks or the commit fail, the Makefile automatically rolls back
the version bump — it is safe to re-run `make release-*` after fixing
the issue.
