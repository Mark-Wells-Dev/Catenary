# Changelog

All notable, human-curated changes to Catenary are recorded here. The format
is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Per-release binaries and auto-generated commit notes are published on the
[GitHub releases page](https://github.com/TwoWells/Catenary/releases); this
file records the curated highlights and, for major releases, the migration
guidance.

## [2.0.0] - 2026-06-08

2.0.0 stabilizes Catenary as a multi-surface intelligence router: a single
per-host daemon manages a shared pool of language servers and exposes them
through four decoupled surfaces — MCP (queries), hooks (enforcement), the CLI
(editing lifecycle and search), and the TUI (observability).

**Every breaking change in this release is to user configuration.** If you
maintain a `~/.config/catenary/config.toml` or a project `.catenary.toml`, read
the [Migration guide](docs/src/migrating-to-2.0.md)
([published](https://twowells.github.io/catenary/migrating-to-2.0.html)) before
upgrading.

### Breaking changes (configuration)

- **Writes resolve-or-deny.** There is no `allow_file_redirects` knob. Before a
  command runs, the hook resolves the complete set of files it will write — from
  shell grammar (`>`, `>>`, `&>`, heredoc targets), argument convention (`cp`,
  `mv`, `tee`, `sed -i`, `ln`), checkable interpreter programs (`awk`,
  `perl -pe`), or a state query (hook-expanded globs, git's own index). A write
  whose target set resolves is allowed and recorded into the diagnostics batch;
  an opaque one (`> $DYNAMIC`, `python -c`, `xargs sed -i`) is denied with a
  teaching message. fd-dups (`2>&1`, `>&2`) and device sinks (`/dev/null`,
  `/dev/stdout`, `/dev/stderr`) are never writes.
- **`awk` and `sed` removed from the recommended command pipeline.** Their
  programs are checked by the write resolver (a pure filter passes; an in-program
  `system()`/`print > file`, or `sed -i`, resolves or is surgically denied), so
  they are kept out of the position-0 pipeline rather than masked behind a bare
  `awk 'prog'`. Route sweeping edits through the new `catenary sed`.
- **Project `.catenary.toml` `[commands]` enforcement keys are ignored.** Only
  `build` is honored at project scope. `client_enforcement_only`,
  `allow`, `pipeline`, `deny`, `deny_flags`, and
  `guidance` resolve at user scope only (the filter resolves daemon-globally for
  every connected session). Move any project-level allowlist to user config;
  Catenary warns when it sees one of these keys in a project file.

### Added

- **`catenary sed`** — a tracked mass-edit surface for multi-file
  substitutions, with a diff-style preview. Substitutions flow through the
  tracked edit path so the diagnostics batch stays complete (the replacement for
  pipeline `sed`).
- **`[tools].diagnostics_per_page`** (default `50`) — single-shot preview budget
  for `catenary diagnostics`. Overflow is written to a per-session report file
  under the runtime dir, referenced by a trailing
  `… N more — full report at <path>` line.
- **`[tools].diagnostics_severity`** (default `"error"`) — minimum severity that
  marks a `catenary diagnostics` run "dirty" (exit code `1`). With the default,
  the exit code means "does it compile" — warnings still print but exit `0`.
- **`[notifications].threshold`** (default `"warn"`) — documents and exposes the
  minimum severity promoted to user-facing notifications (one of `"debug"`,
  `"info"`, `"warn"`, `"error"`).

### Changed

- The command filter is **allowlist-based**: only explicitly permitted commands
  run; everything else is denied with a dump of the allowed configuration. Read
  and stdout-only tools (`cat`, `head`, `diff`, …) live in `allow`; the
  resolve-or-deny write model, not command blocking, handles redirected writes.

### Migration

See the [Migration guide](docs/src/migrating-to-2.0.md) for per-change
before/after examples and exact remediation steps. In short:

- Delete any `allow_file_redirects` line — the write model is now automatic
  (resolvable redirects just work; opaque ones are denied with guidance).
- Drop `awk`/`sed` from your `pipeline`; use `catenary sed` for sweeps.
- Move any project `[commands]` enforcement keys into
  `~/.config/catenary/config.toml`.

Regenerate the recommended template at any time with `catenary config`.
