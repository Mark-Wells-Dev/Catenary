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
- **`[commands.allow_flags]`** — the allow-side dual of `deny_flags`: a
  per-command whitelist of invocation forms (`perl = ["-i", "-pe", "-e"]`). When
  a command has an entry, an invocation must match at least one listed form or it
  is denied naming them. Forms are positive anchors, cluster-normalized (`-pe` ≡
  `{p, e}`) — an invocation matches when it carries all of an anchor's flags;
  extra flags stay governed by the write model. The lever is policy, never
  soundness: it can only narrow, never re-open a form the resolver denies (an
  unauditable `perl script.pl` is denied whatever is listed), and `deny`/
  `deny_flags` still win. User-level only (ignored at project scope); keys must
  name a command in `allow`/`pipeline`/`build`, and an empty form list is a
  config error. `catenary commands` renders the constraint. The recommended
  config now ships `perl = ["-i", "-pe", "-e"]`.
- **Batteries-included default linters.** `catenary diagnostics` is a
  multi-feeder aggregator: it now ships a default standalone-linter set
  (`defaults/linters.toml`), inherited by any root that does not customize or
  disable lint — mirroring the built-in language servers. **actionlint**
  (`.github/workflows/*.{yml,yaml}`), **yamllint** (`**/*.{yml,yaml}`), and
  **shellcheck** (`**/*.sh` plus shebang routing for extensionless
  `sh`/`bash`/`dash`/`ksh` scripts) run over the modified-file set and merge
  into the diagnostics view. A linter that is not installed is skipped (one
  notify, never a hard error). A `[linter.rule.<name>]` with the same name
  replaces the built-in default wholesale (like `[lsp.server.*]`); a new name
  adds a linter, and any non-blessed name is parsed as SARIF. Defaults
  deliberately overlap language-server coverage (shellcheck vs.
  bash-language-server's wrapped shellcheck) and are dedup'd rather than avoided
  by a "disable X when Y" config opinion.
- **`[linter.rule.<name>].shebangs`** — interpreter basenames (`["bash", "sh"]`)
  that route an **extensionless** script to a linter by its `#!` line, in
  addition to `patterns` path globs. Reuses the same shebang detection as
  language classification; the read is lazy (only when the path globs miss). The
  default `shellcheck` ships `["sh", "bash", "dash", "ksh"]`.
- **`catenary stop` confirms before disconnecting live sessions.** Run in an
  interactive terminal with sessions still connected, it now prints the session
  board first — each session's host, workspace root(s), and connected-since,
  read from the `state.json` snapshot — and asks for confirmation before the
  disconnect; declining exits `0` with the daemon left running. `--force` skips
  the prompt, and a non-interactive stdin (scripts, the documented upgrade flow)
  skips it too. The post-stop reconnect warning is unchanged. `catenary stop`
  remains host-only — the command filter still classifies it as not
  agent-invocable.
- **Docs: "Disabling Catenary per project," one subsection per host.** The
  [Installation guide](docs/src/installation.md) now documents the project-scoped
  opt-out for each host — presented as an option, not a recommendation. Claude
  Code (`.claude/settings.json` `enabledPlugins` `"catenary@catenary": false`)
  and Gemini CLI (`gemini extensions disable catenary --scope workspace`) each
  have a clean per-project switch; OpenCode and Antigravity have no native
  per-plugin disable, so the guide states that plainly and gives the file-removal
  path instead. Each subsection spells out what stops (hooks — enforcement,
  command filtering, tracking — plus MCP wiring where the plugin carries it) and
  what remains (the daemon keeps serving other projects, nothing to uninstall,
  re-enabling resumes cleanly because editing state is per-session).
- **Session-start payload flags a stale daemon.** When the session-start
  teaching payload is emitted and the serving daemon runs a different build than
  the CLI, the payload now opens with one restrained note — the daemon's
  behavior may predate the current docs, so observations should be treated as
  potentially stale. It reuses the `catenary version` `tool/version` probe (same
  short timeout, no new probing) and rides every surface identically:
  `catenary primer`, the Claude `SessionStart` / `SubagentStart` context, and
  the raw OpenCode payload all carry it byte-equal, because the line lives in the
  shared payload body. A current daemon adds no line (zero cost); an unreachable
  or unresponsive daemon is left to the existing degraded-payload path with no
  second warning. The note informs evidence quality only — it does not instruct
  a restart, since the daemon lifecycle is host-only.

### Changed

- **Write-resolver denials rewritten for a cold agent.** Every denial from the
  write resolver (an opaque redirect target, `git -C`/`--work-tree`, `sed`/`awk`/
  `perl` writers and computed programs, `xargs`-driven targets, `dd`/`install`/
  `truncate`, and the rest of the closed ~65-message set) now states its cause
  in plain terms for an agent with no Catenary background — "the hook can't tell
  which file this targets" rather than "the resolver doesn't model X" — offers
  the sanctioned way to proceed as one principled clause, and closes with the
  decision-023 pointer to `catenary commands`. Where the proceed clause names
  another shell tool (`sed -i`/`perl -i -pe` for an in-place edit, `cp`/`mv` for
  a copy), it is computed against the live allowlist and names only tools the
  agent may run, falling back to the always-available host edit tools. No change
  to which commands are denied — this is a message sweep only.
- **SessionStart / SubagentStart inline the full prevention payload.** The
  Claude Code session hooks no longer inject a pointer ("run `catenary
  primer`") — they inline the primer's content directly into
  `additionalContext`: the live allow / pipeline / deny surface (resolved from
  the config at emission time), the workflow invariants (the edit→diagnostics
  loop, bare-only vs pipe-friendly commands, the glob quoting / pattern-path
  form), and compact `grep`/`glob` flag synopses with `--help` breadcrumbs.
  `catenary primer` remains and now renders that identical payload from one
  shared module (`src/cli/teaching.rs`), so the on-demand command and the
  pushed hook context cannot drift; `catenary commands` is unchanged.
  `SubagentStart` adds a per-agent diagnostic-debt line.
- **OpenCode joins the runtime-sourced teaching column.** `catenary hook
  session-start --format=opencode` emits the same SSOT payload (the live allow
  surface + invariants + flag synopses) as raw text, and the OpenCode plugin's
  `config` hook regenerates a runtime instructions file from it and registers
  the path on `config.instructions`. OpenCode re-reads that file into every
  request's system prompt, so the live surface rides every request and survives
  compaction with zero per-request work — mirroring Claude Code's SessionStart.
  The shipped
  [plugins/catenary-opencode/catenary.md](plugins/catenary-opencode/catenary.md)
  demotes to a bootstrap/fallback: regenerated from the same SSOT with runtime data
  structurally excluded (no allow surface, build tool, or roots), it covers the
  cold window before the plugin runs and plugin-disabled installs. A freshness
  gate pins the shipped file to the SSOT so the two cannot drift.
- **Gemini CLI joins the runtime-sourced teaching column.** `catenary hook
  session-start --format=gemini` was already registered but withheld the payload
  behind a Claude-only gate; it now emits the same SSOT teaching body Claude
  receives through `hookSpecificOutput.additionalContext` (Gemini injects it as
  the first turn in history). The resume-skip is shared: Gemini restores the full
  prior transcript on `--resume`, so the payload is re-injected only on
  `startup`/`clear`, never on `resume`. The shipped
  [gemini-context.md](gemini-context.md) demotes from a `catenary primer` pointer
  stub to a bootstrap/fallback: regenerated from the same SSOT as the OpenCode
  fallback with runtime data structurally excluded (no allow surface, build tool,
  or roots), pinned by the same freshness gate, it is the compaction-proof
  baseline (Gemini's `SessionStart` has no `compact` source and `PreCompress`
  cannot inject — an accepted, documented gap, not papered over with per-turn
  re-injection).
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
- **`catenary glob` pattern results open with a cardinality header.** Each glob
  **pattern** argument now leads with one line — `N files match <pattern>`
  (singular grammar for one: `1 file matches <pattern>`) — printed *before* its
  per-file listings, so a `| head`-truncated view still shows the true count
  even when the first file's outline overflows the pipe. The header echoes the
  pattern's original spelling, mirroring the zero-match report; the count
  agrees with `--count`. Directory and single-file arguments are unchanged (a
  directory renders its own structure, a named file is its own answer), and a
  zero-match pattern keeps the loud `no matches for pattern` line.
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
- **`catenary diagnostics .` — whole-workspace scope.** Naming a whole tracked
  root routes by capability: when the covering language server advertises
  `workspace/diagnostic`, Catenary serves the root with **one** whole-workspace
  request off the server's existing project model — no per-file open/close
  churn, and it surfaces cross-file diagnostics a per-file pull can miss. A
  server without that capability, or any sub-root directory, transparently
  falls back to the per-file pass (same results). Because a whole-root run can
  span many files, the receipt **collapses the clean files to a count**
  (`N files clean`) and lists only the files with diagnostics; the diagnostics
  themselves always print in full. The edit-loop receipt (a handful of files)
  stays per-file, with `[clean]` beside each clean one.
- The command filter is **allowlist-based**: only explicitly permitted commands
  run; everything else is denied with a dump of the allowed configuration. Read
  and stdout-only tools (`cat`, `head`, `diff`, …) live in `allow`; the
  resolve-or-deny write model, not command blocking, handles redirected writes.

### Fixed

- **`catenary diagnostics` never renders silence for an out-of-root or
  nonexistent named path.** A scoped `catenary diagnostics <path>` that named a
  path outside every mounted root — or one that did not exist (a relative path
  resolving against the wrong cwd) — used to drop it in the path-resolution/
  root-awareness gate and render completely empty stdout, indistinguishable from
  a hang. Every named path now renders exactly one receipt line: `path does not
  exist` for a missing path; `[no language servers running for <root> — not a
  mounted root; see 'catenary roots -h']` when an enclosing project root is
  detectable (walking `.git` up from the path), naming what a `catenary roots
  add` would mount; or a plain `[outside every mounted root; …]` line when no
  enclosing project root is found. The lines compose with the unavailable-server
  banner and never touch the recovery machinery.
- **A bare `catenary diagnostics` after only out-of-root edits no longer lies
  with `[no edited files]`.** An out-of-root edit that arrived with no covered
  edit alongside it never entered editing mode, so the filtered-edit counter
  (which was a no-op until an editing entry existed) never incremented — the edit
  vanished and the next bare run reported `[no edited files]` as if nothing had
  been touched. The filtered edit now records itself even when it stands alone,
  so the run surfaces `(N edits outside tracked roots — …)`; where the edit's
  enclosing project root is detectable the note names it (`no language servers
  running for ~/Projects/Lattice`).
- **`catenary doctor` migration guidance now actually prints, in one pass.** The
  doctor migration walker raw-parsed each config source with a value-expression
  parser (`toml`'s `FromStr for Value`, which parses `[1, 2]`/`{a = 1}`, not a
  document), so it failed on every real config and silently skipped every
  source — the guard error pointed at guidance that never appeared. The walker
  now document-parses each source and renders the rename block for every stale
  namespace (`[server.*]` / `[language.*]` / `[linter.<name>]`) in a single run.
  The guard error itself now names every present old class at once (no
  rename-rerun-rename loop), and doctor's own render drops the self-referential
  "run `catenary doctor`" pointer now that the guidance prints directly above it
  (daemon startup and `catenary commands` keep the pointer).
- **`catenary doctor` no longer warns about orphaned embedded-default servers.**
  The "defined but not referenced" warning fired for built-in default server
  definitions left unrouted by a user `[lsp.language.*]` override — normal
  operation, not user error (16 spurious warnings on a maintainer config). It
  now warns only for user-defined servers nothing routes to.
- **`catenary diagnostics` no longer prints empty stdout when every file's
  server produced no result.** A file whose language server died mid-pipeline
  (or never answered) resolves to no result, so it earns neither a `[clean]`
  line nor diagnostics — and an all-such set used to render as completely empty
  stdout, indistinguishable from a hang or silent failure, while still clearing
  the gate. Each unverified file now appears in the receipt as an explicit
  `[unverified — <server> returned no result]` line (a directory/whole-root
  scope folds them to an `M files unverified` count alongside the clean count).
  Render-only: whether an unverified file pays its editing debt is unchanged.
- **A lint-only file whose linter ran and found nothing now earns `[clean]`.**
  The linter feeder skipped empty results, so a lint-only file (e.g. a
  shellscript routed to shellcheck) that its linter verified clean produced no
  feeder entry, classified `NoResults`, and — carrying no assigned server —
  stayed out of the receipt entirely. An empty lint result is a verification, not
  an absence: the feeder now records ran-and-found-nothing as an empty result so
  the file classifies clean, mirroring the language-server path's record-even-
  with-zero-diagnostics rule. Lint-clean files join the `[clean]` list and the
  `N files clean` collapse with no linter-specific wording. A linter that never
  completed (not installed, spawn or parse failure) still records nothing and
  leaves its file unverified — only an actual run verifies.
- **`perl` is a nicer sed, not a script host.** `perl` is allowlisted for its
  text-processing role — inline checkable `-e`/`-E` substitutions only. A perl
  invocation with **no** literal program is now denied: both the script-file
  shape (`perl script.pl`, with or without `-p`/`-n`) and the stdin-program
  shape (bare `perl` in a pipeline), and the same under `xargs`. A script the
  hook can't see would run in-script `open`/`unlink`/`system` writes
  unattributed and read files outside `catenary grep`/`glob` — the decision-026
  gap. The denial names the inline checkable form (`perl -i -pe 's///`, config
  permitting) or the host edit tools. Pure introspection (`perl -v`/`-V`/`-h`
  with no file operands) still passes; `perl -e`/`-pe` filters and `perl -i`
  in-place edits are unchanged.
- **An unavailable diagnostics server now degrades its files' coverage instead
  of silently costing them.** A language server that dies mid-run — or fails to
  start at all (the "dies during `initialize`" class) — leaves its files
  unverified. Catenary now makes **one bounded, in-run attempt** to respawn the
  server and re-run the unretrieved remainder of its batch (bounded by the same
  spawn/initialize budgets — a slight stall, never an unbounded wait); if that
  recovers, the files are verified normally. If it fails, coverage has
  *degraded*: coverage is **effective, not nominal**, so a server that cannot be
  brought back means its files owe nothing for this run — the same class as a
  file no server covers, because the gap is Catenary's to close, never the
  agent's. The receipt now **opens with a top-line banner naming the unavailable
  server** (`unavailable: <server>`), with the per-file `[unverified — <server>
  returned no result]` lines (and the `M files unverified` collapse) beneath it,
  so degraded never reads as clean. The exit stays `0` and the editing gate
  drains the degraded file exactly as a paid one — no armed states, retry
  counters, or cross-run tracking; editing the file again re-arms it, and a
  server that is back next run resumes the normal contract.
- **Diagnostics settle no longer reports `[clean]` over a CPU-starved flycheck
  child.** Under host CPU saturation a runnable child can present an all-zero
  50 ms sampling window — no user/system CPU or page-fault deltas while ticks
  of work remain queued — and the idle detector read that single window as
  idle, settling early and withholding the child's diagnostics. The quiet
  predicate now consults the scheduler state Catenary already samples per
  process: a live process that is running (on a core or in the run queue) or
  blocked (uninterruptible kernel I/O) keeps the settle open regardless of
  deltas, and idle requires zero deltas, no new processes, and every live
  process in a sleep-class state. Pressure-independent by construction — no
  timing heuristics and no CPU budget. On platforms without observable
  scheduler state (Windows), detection falls back to CPU deltas as before.

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
