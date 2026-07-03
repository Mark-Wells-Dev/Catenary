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

- **Subsystem config namespacing.** Definitions now live under their subsystem
  table so each subsystem is one self-contained section (`disable` + its
  config): `[server.*]` → `[lsp.server.*]`, `[language.*]` → `[lsp.language.*]`,
  and `[linter.<name>]` → `[linter.rule.<name>]`. The `[lsp]` table now carries
  the per-root `disable` toggle alongside `[lsp.server.*]` / `[lsp.language.*]`,
  and `[linter]` carries `disable` alongside `[linter.rule.*]`. Co-located
  diagnostic-weight fields ride along: `[lsp.server.<name>].weight`,
  `[lsp.server.<name>.sources]`, and `[lsp.server.<name>].provisional`. The old
  top-level `[server.*]` / `[language.*]` / `[linter.<name>]` forms are now a
  hard error naming the exact rename; `catenary doctor` flags each stale table
  header (nested sub-tables like `[lsp.server.<name>.sources]` included).
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
  `awk 'prog'`. Native `sed`/`perl` are on the recommended allowlist as top-level
  writes: sweep with `sed -i` (or `perl -i -pe` for look-around/back-references),
  whose write-set the resolver records into the diagnostics batch — an opaque
  write (`sed -f script`, an in-program `w`/`e`, a computed `$VAR` script) is
  surgically denied.
- **Project `.catenary.toml` `[commands]` enforcement keys are ignored.** Only
  `build` is honored at project scope. `client_enforcement_only`,
  `allow`, `pipeline`, `deny`, `deny_flags`, and
  `guidance` resolve at user scope only (the filter resolves daemon-globally for
  every connected session). Move any project-level allowlist to user config;
  Catenary warns when it sees one of these keys in a project file.

### Added

- **`[tools].diagnostics_severity`** (default `"error"`) — minimum severity that
  labels a `catenary diagnostics` run "dirty" (vs "clean"). A status label only:
  the run always exits `0` and prints every diagnostic; it does not gate an exit
  code.
- **`[notifications].threshold`** (default `"warn"`) — documents and exposes the
  minimum severity promoted to user-facing notifications (one of `"debug"`,
  `"info"`, `"warn"`, `"error"`).

### Changed

- **`catenary glob` outlines show types and callables only.** The file and
  directory outline is now a map, not a mirror: it recurses into containers
  (modules/namespaces/packages and classes/interfaces/enums/structs/impls) and
  shows their functions, methods, and constructors, but prunes data members
  (fields, properties, enum variants, and below-top-level variables/constants)
  and never enters a callable's interior (locals, loop vars, nested defs — each
  callable renders as one line). The top level still shows everything, so a
  module-level constant remains. Only the outline render filters — the symbol
  index stays complete, so `catenary grep`'s `#scope` symbol-path enrichment is
  unaffected.
- **`catenary glob` teaches the quoted-pattern form and never expands
  silently.** `catenary glob --help` and the primer now explain that a PATH may
  be a quoted glob pattern, absolute or cwd-relative, with the anchor written
  into the pattern (`catenary glob 'src/**/*.rs'`) — there is no separate
  directory argument. A pattern argument that expands to zero matches is now
  reported loudly per argument as `no matches for pattern: <pattern> (relative
  patterns anchor at cwd)`, even when a sibling argument renders — superseding
  the old whole-result `no files matched` line for glob.
- **`catenary diagnostics` exit contract + per-file receipt.** The command now
  exits `0` whenever it ran correctly — clean *or* dirty — and `2` only on a
  genuine fault (no daemon, IPC failure); it **never** exits `1`. The exit code
  is a trust signal for the agent harness ("did the call succeed?"), not a lint
  result, so a dirty run is no longer misread as a failed call. The clean/dirty
  distinction moves entirely into stdout as a **per-file receipt**: every
  diagnosed file is listed, `[clean]` beside the clean ones and diagnostics
  beneath the dirty ones (`[no edited files]` for a genuinely empty set). This
  retires the old silent-on-clean behavior — a clean file is now stated
  explicitly, never inferred from empty output.
- **`catenary diagnostics [paths…]` — scoped, on-demand lint.** Paths are now
  first-class: bare still reports and clears the whole edited set, but naming
  paths diagnoses exactly those (relative to the shell's cwd) and pays only
  their share of the edit gate. The gate is a debt paid by *diagnosing*, not
  fixing — a partial pull leaves the gate armed for the files you didn't name,
  and editing a paid file re-arms it. A named path that was never edited is
  simply linted, paying nothing. This retires the old accept-and-warn note that
  told you paths "take no arguments" and were ignored.
- **The editing-gate message teaches the pull, named by feeder.** When a
  command is held because edits are still undiagnosed, the message now reads as
  a helpful next step rather than a fault: it lists the outstanding files
  **grouped under each diagnostic feeder** (the LSP server or linter that
  tracks each) and teaches both ways to clear them — bare `catenary
  diagnostics` (all) and `catenary diagnostics <those files>` (scoped, shown
  with your actual outstanding paths) — then names the command to re-run. No
  inferred intent, no internal jargon: just the files that haven't been
  diagnosed yet and the exact command to diagnose them.
- The command filter is **allowlist-based**: only explicitly permitted commands
  run; everything else is denied with a dump of the allowed configuration. Read
  and stdout-only tools (`cat`, `head`, `diff`, …) live in `allow`; the
  resolve-or-deny write model, not command blocking, handles redirected writes.

### Migration

See the [Migration guide](docs/src/migrating-to-2.0.md) for per-change
before/after examples and exact remediation steps. In short:

- Delete any `allow_file_redirects` line — the write model is now automatic
  (resolvable redirects just work; opaque ones are denied with guidance).
- Drop `awk`/`sed` from your `pipeline`; sweep with native `sed -i` /
  `perl -i -pe` (the resolver records their writes into the diagnostics batch).
- Move any project `[commands]` enforcement keys into
  `~/.config/catenary/config.toml`.

Regenerate the recommended template at any time with `catenary config`.
