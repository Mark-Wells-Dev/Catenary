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

- **Guided install: a blessed suggestion is one consented keypress from
  working coverage.** On a suggestion row whose server has a
  blessed-manifest entry matching its recipe pin, `a` opens the consent
  overlay showing exactly what will run — package, pinned version, the
  fetch URL, and the verification step — before anything executes. npm
  installs fetch the pinned tarball, verify its sha512 against the recipe
  (a mismatch aborts before any install), and install the verified
  artifact with `--ignore-scripts`; cargo uses `--locked` version pins;
  go rides the checksum DB; pip requires hashes or politely refuses.
  Everything is spawned as literal argv — no shell strings, no curl|bash.
  Blessing is structural: an unblessed or version-skewed recipe cannot
  construct an install action at all, and the shipped manifest starts
  empty — nothing is offerable until CI conformance blesses it. On
  success the health model re-probes and the suggestion vanishes. The
  engine is user-surface only; agents get no install verb.
- **Guided mutations: the dashboard's fix-its become executable.** On a
  broken-server problem row or a cursored server, `a` opens a consent
  overlay showing exactly what will be written — key, value, target file —
  before anything happens; Enter applies, Esc declines, no write is ever
  silent. v1 actions: set a server's binary path, enable/disable a server,
  and apply the config-namespace migration renames. Writes are surgical
  (`toml_edit` — a hand-formatted config with comments survives byte-
  identical outside the edited key) and **layer-routed structurally**:
  project-scoped keys may target a root's `.catenary.toml`, everything
  else targets user `config.toml`, and enforcement keys can never yield a
  project write — the TUI cannot author config the engine ignores.
  Applied changes surface an honest `pending daemon restart` marker in
  the problems pane until a restarted daemon's identity confirms the new
  config is live; provenance re-reads the file just written, never
  assumes.
- **Language-server conformance: every shipped server definition is now a
  testable claim.** A conformance harness drives the real Catenary
  lifecycle — spawn through the shipped defaults with live
  `workspace/configuration` delivery, edit, settle, collect, clean
  shutdown — against a per-language fixture carrying one intentional
  diagnostic, under a generous-but-finite wall bound that catches the
  never-yields class (the pyright 44-minute incident is the type
  specimen). The dogfooded fleet runs as always-on sentinels in ordinary
  CI; a matrix workflow conformance-tests any recipe change one server
  per isolated job. Shipped server definitions gain **pinned install
  recipes** (exact version + content hash per ecosystem's verification
  tier — npm tarball sha512 verified before an `--ignore-scripts`
  install; cargo `--locked`; go checksum-DB; all marked draft) and a
  **blessed-manifest** that alone can ever feed a user-facing
  recommendation — drafts structurally cannot. `make refresh-recipes`
  re-resolves and re-hashes pins into a reviewable diff. No retry
  absorption anywhere: a server that loses its first publish to the
  cold-pull race fails loudly (that race is bug 74, a real settle/pull
  gap the harness itself discovered).
- **Non-git agent worktrees: Subversion and Mercurial projects get real
  isolated working copies.** Where the project is `.svn` or `.hg`, the
  WorktreeCreate hook now creates a genuine working copy under the same
  agents scheme instead of refusing — svn via a fresh `checkout` of the
  working copy's URL (recording `URL@revision` in the sidecar; note svn
  checkouts don't share the source's store, so local uncommitted changes
  don't carry over — surfaced at creation, not hidden), hg via `hg share`
  (the true worktree analog, extension enabled inline) with `hg clone` as
  the fallback. Disposal dispatches per VCS with the same guarantees and
  simpler proofs: svn clean = `svn status` empty (no local-commit class,
  so no unpushed leg); hg clean = status empty and no draft changesets
  beyond the recorded base; a clean copy is a plain directory delete — no
  branch or registration legs. The armed WorktreeRemove handler — dormant
  for git, where the host never fires it — is live for non-git from day
  one, same scheme-and-sidecar guard, same dirty refusal.
  `.worktreeinclude` and `catenary worktree rm` work across VCSes; feats
  stay git-only; `.jj` keeps the named refusal (no binary to verify
  against).
- **Catenary owns the agent-worktree lifecycle end to end: the
  `catenary worktree` surface and guarded disposal.** Two worktree classes:
  *agent* worktrees (hook-created isolation copies) and *feats* worktrees —
  deliberate long-lived parallel checkouts created by
  `catenary worktree add <branch>`, placed under the durable state base with
  a human-friendly sibling symlink (`<repo>-<branch>`) beside the main repo.
  `catenary worktree ls` lists every tracked worktree with class, creator,
  age, clean/dirty, and mount state (feats also show ahead/behind their
  remote); `catenary worktree rm` is the one removal verb — for agent
  worktrees the caller's captured-work assertion replaces the clean proof,
  for feats it refuses anything uncommitted or unpushed. Raw `git worktree`
  is denied on the agent surface with a pointer at the sanctioned commands
  (which also closes the last manual route to in-repo worktree nesting).
  Clean agent worktrees now dispose automatically — at subagent stop, at
  session end, and via a 24-hour age sweep — through one guarded routine:
  provably clean only (pristine status AND HEAD at the recorded base
  commit), always via `git worktree remove` (never force, never touching
  locks — git's refusal outranks Catenary's own checks), branch names taken
  from the creation sidecar, the sidecar deleted last as the transaction
  record so any interruption converges on the next sweep. Dirty worktrees
  are never auto-deleted, at any age, by any trigger: a subagent leaving
  unlanded work notifies the parent session at the moment it stops, a
  lingering worktree nags once per session at the top-level stop (never
  while its agent runs in the background or waits at a permission prompt),
  and orphans from previous sessions get a session-start line pointing at
  `catenary worktree ls`.
- **`catenary query` joins the agent command surface.** The read-only
  telemetry query command is now agent-invocable — pure observability, so it
  classifies with `grep`/`glob`: it chains and pipes out freely and needs no
  isolation. It reads no stdin, so piping *into* it is denied with a teaching
  message instead of silently doing nothing. Previously agents got the
  unknown-subcommand denial.
- **Agent worktrees live outside the repo.** The Claude Code plugin now
  registers a `WorktreeCreate` hook (`catenary hook worktree-create`): a
  `--worktree` session's or `isolation:"worktree"` subagent's working copy is
  created out of tree — under the durable state base at
  `catenary/worktrees/agents/<session>/<agent|name>` with a metadata sidecar
  recording its origin — instead of nested
  under `.claude/worktrees/` inside the repo — so the parent root's language
  servers can never descend into it and double-index the project
  (rust-analyzer's gitignore-blind cargo discovery). The subagent's own
  server mounts the relocated worktree exactly as before (the SubagentStart
  mount and the deletion watcher are location-agnostic). Git tracks the
  relocated worktree by metadata, so `git worktree remove` handles it
  unchanged — but in live testing Claude Code never ran its documented
  automatic cleanup for a hook-created worktree; Catenary therefore owns
  teardown and disposal itself (see the `catenary worktree` entry and the
  subagent-stop reap entry). The hook forwards its payload
  to the firehose (`catenary query --kind hook`) so the live schema is
  verifiable. Because a configured WorktreeCreate hook replaces the host's
  default behavior entirely, the hook **reimplements `.worktreeinclude`**
  (gitignore-syntax patterns; matching local files like `.env` are copied
  into the new worktree, existing checked-out files never clobbered) — no
  host feature is lost. Non-git working copies originally got an honest
  named refusal (`.svn`/`.hg`/`.jj` detected and named) instead of a raw
  git error; later in the release `.svn`/`.hg` graduated to real working
  copies (see the non-git worktrees entry) and only `.jj` keeps the
  refusal.
- **`grep` flag muscle-memory.** `-n`/`--line-number` and
  `-H`/`--with-filename` now parse and do
  nothing — line numbers (`path:N`) and filenames are unconditional in the
  output, so the `grep`/ripgrep muscle-memory reach succeeds instead of
  erroring. `-c` joins as a hidden ripgrep-letter short for `--count`.
  Suppressor spellings we don't honor (`--no-line-number`, `--no-filename`)
  stay an honest unknown-flag error. Every existing short (`-i -s -w -F -v -l
  -A -B -C -g -t`) becomes a hidden `short_alias`: it keeps working, but
  `catenary grep --help` and the teaching surface now show long forms only.
- **JSON Schema for the config files.** Catenary now ships two
  `schemars`-generated JSON Schemas under `schemas/` — `config.json` for the user
  config (`~/.config/catenary/config.toml`) and `catenary-project.json` for a
  project `.catenary.toml`. Both are generated from the same serde structs the
  loader deserializes, with a byte-for-byte freshness gate so they cannot drift.
  The schema closes the key set (a dead key like `smart_wait` is flagged
  in-editor) while leaving the server pass-through subtrees
  (`initialization_options`, `settings`, `env`) open; the project schema marks the
  user-scope-only enforcement keys (`allow`, `deny_flags`, …) deprecated so the
  editor warning matches the runtime warning. Delivery is validator-neutral: the
  schema is published at
  `https://twowells.github.io/catenary/schemas/config.json`, auto-associated at
  the `taplo` server Catenary spawns (offline, zero setup), and referenced from a
  `#:schema` directive in the `catenary config` template.
- **`catenary doctor` flags unknown config keys.** Doctor now walks each user
  config source against the shipped JSON Schema — the same known-key SSOT taplo
  validates against, so the two surfaces can never disagree and doctor inherits
  every future key for free — and warns per unrecognized key with its location
  (`` `smart_wait` (top level) ``, `` `typo_key` (in [tui]) ``). A dead knob left
  over from a past rework no longer sits silently accepted. Openness is read from
  the schema, so the server pass-through subtrees (`initialization_options`,
  `settings`, `env`) and the wildcard-keyed maps (`[lsp.server.*]`,
  `[roots.companions]`) never false-positive. Unknown keys warn, never error: an
  older binary reading a newer config keeps working.
- **Activity-mounted ephemeral roots.** A `catenary grep`, `glob`, or
  `diagnostics` touching a path outside every mounted root now detects the
  enclosing project root (walking `.git` up from the path), **mounts it
  ephemerally**, and serves the enriched/diagnosed result from the
  freshly-attached server(s) — no `catenary roots add` required. The mount
  **expires after a few minutes of inactivity**; every qualifying activity
  (search, outline, diagnostics, edit tracking) refreshes its idle clock, so an
  active root never expires out from under you and an idle one cleans itself up.
  `catenary roots add` on an ephemerally-mounted root **upgrades it to pinned**
  and stops the expiry; `catenary roots ls` and the `state.json` root board
  distinguish the two classes. Only the single enclosing root mounts (never a
  sibling), and companion-root templating does not apply to ephemeral mounts.
  This supersedes the out-of-root receipt-honesty fallback wherever a root is
  mountable; the fallback still answers for unmountable paths.
- **`[tools].diagnostics_severity`** (default `"error"`) — minimum severity that
  labels a `catenary diagnostics` run "dirty" (vs "clean"). A status label only:
  the run always exits `0` and prints every diagnostic; it does not gate an exit
  code.
- **Diagnostics receipts persist per session.** The daemon now writes the full
  rendered `catenary diagnostics` receipt to a per-session store under
  `runtime_dir()/catenary/receipts/` at **compute time**, before the response
  leaves for the client — so a CLI invocation killed after dispatch (a
  backgrounded command reaped by the host, a tool-call timeout, a Ctrl-C) can no
  longer pay the editing debt without anyone seeing the receipt. Every run prints
  a trailing pointer line naming the store and the files it covered; a later bare
  `catenary diagnostics` that finds nothing edited (`[no edited files]`) still
  names the prior store and its covered set, so the agent can tell at a glance
  whether it holds the set just edited or a stale one. The store is regenerable
  ephemera (same tmpfs lifecycle as `state.json`); a failed store write is
  fail-soft and never breaks the receipt. Closes the killed-client witness gap.
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
- **`[commands] script_hosts`** — a user-level opt-in that lets a modeled
  substitution engine (`perl`/`awk`/`sed`) run its **script-file** form as a full
  script host: `script_hosts = ["perl"]` re-classes `perl script.pl` (and the
  bare stdin-program shape, plus `awk -f`/`sed -f`) from the misc-126 soundness
  denial to the unbounded-interpreter executor boundary (`NoWrite`) — the same
  layer-4 stance `python script.py` keeps. Inline `-e`/`-E` code still faces the
  substitution audit (`perl -e 'print 1'` stays denied), and `perl -i` still
  resolves its write-set. The layers compose default deny → `script_hosts`
  (executor boundary) → `allow_flags` (narrowing); a command in both
  `script_hosts` and `allow_flags` is a warned contradiction. User-level only
  (ignored at project scope); keys must name a command in `allow`/`pipeline`/
  `build`, listing an already-unbounded interpreter is a warned no-op, and an
  empty list is a config error. `catenary commands` renders a `Script hosts:`
  line. The default (key absent) is byte-identical to prior behavior.
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

- **The notification queue retires: three channels, each doing what it's
  for.** The store-and-forward `systemMessage` queue — which could deliver
  a warning minutes after its problem was already fixed — is gone. In its
  place: **errors** fire an OS desktop notification (the urgent interrupt,
  unchanged) and surface as health findings; **warnings** persist as TUI
  health findings until fixed, with no interrupt; **everything** lands in
  the firehose for `catenary query`. The one queue-fed message whose real
  audience was the *agent* — "your subagent left a dirty worktree" — now
  reaches the spawning parent directly via hook-response
  `additionalContext` on its next tool use or stop (session-scoped,
  top-level-parent only), instead of telling the user about work only the
  agent can land. The `[notification] threshold` key retires with the
  queue (a leftover key is flagged by the unknown-key finding, not a
  startup failure), and the tracing conventions are re-trued: an
  `error!()` must earn its interrupt.
- **The TUI is the health dashboard: a master-detail grid replaces the
  four-board monitor.** Bare `catenary` now opens a 2×2 grid built to
  answer *"is it working?"*: a root-grouped server tree (one collapsed
  line per healthy root; contributor classes, idle countdowns, respawn
  history, dormant inventory behind a toggle), a client-grouped session
  tree (install-health findings inline at the client, live subagents as
  sub-rows, honest `last seen Nm` when a host's hooks can't say more), a
  contextual detail pane (a server shows its effective config by layer
  with provenance; a root its routing table; a session its recent
  actions), and a **problems pane** — every Fatal/Error/Warning finding
  with its fix-it, selecting one focuses the board on its owner. The
  header carries the verdict, version skew, and snapshot staleness;
  suggestions ride as a collapsed tail that never displaces a problem
  and never dents a green verdict. Keyboard-first with equal mouse
  support, a problems-only filter, and the same unwedgeable
  `state.json` watch+poll underneath — the TUI still never probes and
  never opens the firehose. The severity ladder behind it gains
  **Fatal** (a routed server you configured or installed that isn't
  running — a missing binary counts when the server is explicitly
  configured) and **Suggestion** (a live language with a known default
  server and nothing installed — named, never shouted, and never an
  unpinned install command); `catenary doctor` renders the same tiers.
  The dead SQLite-era `[tui]` config keys are retired (they now surface
  through the unknown-key finding), and the session snapshot carries
  live subagents per session.
- **One health model: doctor's checks extracted into a typed findings
  library, and dormant servers stop presenting as errors.** Every check
  `catenary doctor` performs now resolves in `src/health/` into typed
  findings — severity, a stable machine-readable code, the message, and
  fix-it guidance as data — with doctor reduced to a one-shot renderer
  (its tests pin finding codes, not prose). The library distinguishes
  **routed** servers (a language binding routes to them and the language
  is live in a tracked root) from **dormant inventory** (configured,
  routed-to by nothing): a missing binary for a dormant server is now an
  informational inventory line, not an error — the "two broken servers
  drowning among fifty dormant defaults" report reads clean. Version-skew
  detection joins the model. The same findings feed the upcoming TUI
  health dashboard through a feed seam (doctor probes daemon-down; the
  TUI will read live daemon state) — the two renderers can never diverge
  in what they know.
- **The session board tells the truth: respawn history, teardown removal,
  degradation state, contributor classes.** Four `state.json` enrichments
  (schema 2, additive — old readers unaffected) that make the snapshot fit
  to back a health dashboard. A respawn no longer rebirths the server entry:
  a crash-looping server now shows its climbing `respawns` count and
  `last_died_at` instead of posing as a healthy young process. A per-root
  reap (worktree teardown, idle expiry, session end) removes that root's
  server entries from the board — previously they persisted as healthy
  ghosts with frozen state after the servers were verifiably gone. When the
  diagnostics path degrades a server's files to uncovered (decision 027),
  the entry records `degraded_since` and the reason, cleared on recovery —
  that state previously lived only in the receipt banner. And root entries
  carry their full contributor classes (`hook`/`mcp:*`/`worktree:*`/
  `ephemeral:*`) plus the ephemeral idle countdown, instead of one
  collapsed boolean.
- **Root management renamed to what it does: `catenary pin` / `catenary
  unpin`.** Coverage became automatic (activity automount, worktree mounts,
  MCP roots), so `roots add` never granted coverage — it pinned: stop idle
  expiry, pre-warm servers, upgrade an ephemeral mount. The commands now say
  so: `catenary pin <path>` / `catenary unpin <path>` replace
  `roots add`/`roots rm` (the old spellings return a teaching error naming
  the new one), and bare `catenary roots` lists the tracked roots with their
  contributor classes (`roots ls` stays as a working alias). `unpin` matches
  the stored path, so unpinning a directory that no longer exists on disk
  works and repeating it is a no-op (bug 54). The agent primer teaches none
  of this — root lifecycle is not the agent's job; in its place, one
  capability fact: parallel agents belong in isolated worktrees, because two
  agents sharing a workspace also share its language servers and break
  settle detection.
- **Root detection is repo-shaped, not git-shaped.** The
  enclosing-project-root probe — the anchor behind ephemeral automount,
  out-of-scope diagnostics receipts, and out-of-root edit notes — now
  recognizes `.svn`, `.hg`, and `.jj` markers alongside `.git` (dir or
  file), so Subversion, Mercurial, and Jujutsu projects get automount and
  ordinary `grep`/`glob`/`diagnostics` coverage. Ignore semantics remain
  gitignore-based for now.
- **Subagent worktree roots unmount at agent completion — identity-keyed,
  outcome-gated, idle-bounded.** When a worktree-isolated subagent truly
  stops (the stop gate *allows* — a blocked stop leaves the root warm for
  the mandated diagnostics run), the daemon reaps its worktree root and
  shuts down that root's language servers, instead of leaving a full server
  set (rust-analyzer included) resident until the parent session ends. The
  mount now keys on agent identity (`worktree:{session}:{agent_id}`) rather
  than a drift-prone working directory: creation records a metadata sidecar
  (base commit, branch, identity) that a daemon registry rehydrates at
  startup, the stop reap resolves through the registry with an
  enclosing-root cwd fallback for foreign worktrees, and divergences are
  logged. Two backstops bound the no-signal cases: a 30-minute idle timeout
  unmounts worktree roots whose subagent died without a stop event (never
  touching disk), and a new `PermissionRequest` observer hook marks an agent
  waiting at a permission prompt as *blocked* — exempt from idle expiry, so
  a worker paused on a human answer keeps its warm servers for hours. A
  resumed subagent regains coverage on its first search or diagnostics run
  via the ephemeral activity mounts. Disposal of the directory itself is
  in-house too (see the `catenary worktree` entry above) — the host's
  documented automatic cleanup never runs for hook-created worktrees
  (known-issue trail in anthropics/claude-code#34137).
- **`catenary diagnostics` becomes idiomatic — the batch replaces the drain.**
  The tracked set is no longer destroyed at diagnose time. Edits accumulate
  into a persistent per-`(session, agent)` **batch** whose files each carry a
  `delivered` flag: a bare run computes fresh diagnostics over the whole batch
  and flips every flag — but only after the response actually reaches a
  client, so a killed run leaves the gate armed — and a repeat bare run
  re-diagnoses the same batch fresh instead of answering `[no edited files]`
  (stable scope, live truth, the `git status` idiom). Scoped runs flip only
  the named files; the editing gate holds while any flag is false; the first
  covered edit after a fully-delivered batch discards it and starts the next.
  The per-session receipt store and its `last computed diagnostics saved
  to …` pointer line are retired — recomputing over the surviving batch
  supersedes re-reading stored bytes, so killed-client recovery is now just
  "run it again."
- **Grep results stream from bounded daemon memory — output unchanged to the
  byte.** The daemon no longer materializes a whole search result as one
  in-memory `String` plus its JSON-escaped copy. Each file's rendered hunk
  lands in a sorted map as its results complete; past an internal buffer
  threshold, hunks spill to a per-request disk spool under the cache dir
  (unlinked on completion and on cancellation), and the response streams to
  the CLI as chunk frames in the same deterministic (file, line) order,
  terminated by a tally frame. Peak daemon memory is the path index plus one
  hunk in flight, regardless of result size. stdout is byte-identical
  (contract-tested with the spool threshold forced to zero), and version skew
  degrades honestly — either peer lacking the framing falls back to the
  single-envelope response. A daemon-wide search limiter now bounds
  concurrent grep/glob walks so one session's monster search cannot starve
  the pool, and glob's directory walks move off the async runtime
  (`spawn_blocking`, cancel token threaded inside), closing the phase-1
  residual: a single massive directory now cancels mid-walk.
- **The 10 MB size cap falls — content classifies the search.**
  `BINARY_SIZE_THRESHOLD` is deleted: files are classified by a BOM-aware
  quit-at-first-NUL content sniff on every entry path (named, shell-expanded,
  walked, quoted-glob), at any size. A >10 MB pure-UTF-8 file is searched to
  EOF and matched like any other text file (previously
  `skipped (too large (>10 MB))` even when explicitly named), and
  `catenary glob` renders it with a line count and outline; a UTF-16 BOM file
  classifies as text (the BOM check precedes the NUL verdict). The only skip
  left is the honest `skipped (binary): path`, decided by content after about
  one head block. Doctrine: serve the request; guard the daemon — and the
  guard became real: the parallel grep walk now runs on `spawn_blocking` with
  the disconnect-cancel token threaded into every walker visitor
  (`WalkState::Quit`), so a dead client's search actually stops instead of
  reading the tree to completion; `catenary glob` wires its previously-ignored
  cancel token the same way.
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
- **Claude Code's `catenary:primer` skill is dropped.** With SessionStart
  inlining the full payload on `startup`/`clear`/`compact`, the static skill was
  pure duplication; it no longer ships in the plugin and disappears from Claude
  Code's skill list. The `catenary primer` command is unchanged — it renders the
  same payload on demand.
- **OpenCode joins the runtime-sourced teaching column.** `catenary hook
  session-start --format=opencode` emits the same SSOT payload (the live allow
  surface + invariants + flag synopses) as raw text, and the OpenCode plugin's
  `config` hook regenerates a runtime instructions file from it and registers
  the path on `config.instructions`. OpenCode re-reads that file into every
  request's system prompt, so the live surface rides every request and survives
  compaction with zero per-request work — mirroring Claude Code's SessionStart.
  (A shipped static fallback file originally accompanied this; the plugin-only
  reshape below retired it — OpenCode teaching is runtime-only.)
- **OpenCode integration is plugin-only — `catenary install opencode` no longer
  edits `opencode.json`.** The installer now writes exactly one Catenary-owned
  file (`plugin/catenary.js`) and never creates, parses, or rewrites the
  user-owned `opencode.json`. The MCP heartbeat (`mcp.catenary`) that used to be
  merged into that file now rides the plugin's `config` hook, injected
  unconditionally and first (`??=`, so it defers to any existing entry). Teaching
  is **runtime-only**: the `config` hook regenerates the instructions file from
  the binary and registers its path — there is no shipped static fallback,
  because regeneration needs only the `catenary` binary and local config (nothing
  daemon-side), so a regen failure means the install itself is broken and generic
  teaching pointing at dead commands would be worse than silence. This retires a
  class of install failures on comment-bearing / dotfile-managed configs, at the
  cost of collapsing the disable story to one switch — disabling the plugin now
  drops enforcement, teaching, and the heartbeat together. Upgrading users may
  remove the previously merged `mcp.catenary` / `instructions` entries from
  `opencode.json` and delete any old `~/.config/opencode/catenary.md`; leaving
  the merged entries is harmless.
- **Gemini CLI joins the runtime-sourced teaching column.** `catenary hook
  session-start --format=gemini` was already registered but withheld the payload
  behind a Claude-only gate; it now emits the same SSOT teaching body Claude
  receives through `hookSpecificOutput.additionalContext` (Gemini injects it as
  the first turn in history). The resume-skip is shared: Gemini restores the full
  prior transcript on `--resume`, so the payload is re-injected only on
  `startup`/`clear`, never on `resume`. The shipped
  gemini-context.md demotes from a `catenary primer` pointer
  stub to a bootstrap/fallback: regenerated from the SSOT's static tiers
  (`fallback_body()`) with runtime data structurally excluded (no allow surface, build tool,
  or roots), pinned by the same freshness gate, it is the compaction-proof
  baseline (Gemini's `SessionStart` has no `compact` source and `PreCompress`
  cannot inject — an accepted, documented gap, not papered over with per-turn
  re-injection).
- **Antigravity gets first-sighting teaching via `PreInvocation`.** Antigravity
  has no `SessionStart` surface; its `PreInvocation` hook fires before every model
  call. `catenary hook pre-invocation --format=antigravity` injects the same SSOT
  payload Claude/Gemini/OpenCode carry as a **persisted** `injectSteps`
  `userMessage` — the analog of the Claude `SessionStart` `additionalContext` —
  delivered exactly once per conversation, never per turn (the transient
  `ephemeralMessage` channel is deliberately excluded). First-sighting is decided
  daemon-side, keyed on `conversationId`, so it is robust to `invocationNum`
  semantics on resume; the ledger is fail-closed when the daemon is unreachable
  (a skipped injection self-heals on the next reachable call rather than risking a
  duplicate). The shipped
  [plugins/catenary-antigravity/hooks.json](plugins/catenary-antigravity/hooks.json)
  registers the hook; a reinstall (`catenary install antigravity`) picks it up.
  In the same motion the shipped
  [plugins/catenary-antigravity/rules/catenary.md](plugins/catenary-antigravity/rules/catenary.md)
  demotes from a `catenary primer` pointer stub to the generated `fallback_body()`
  render (SSOT static tiers, runtime data structurally excluded), pinned by the
  same freshness gate as gemini-context.md. Rules files re-inject per conversation
  turn, so the file carries explicit `trigger: always_on` frontmatter to load
  unconditionally every turn — making it this host's only compaction-proof teaching
  leg (the persisted `userMessage` above dies at compaction with no observable
  signal). The two now form a hybrid mirroring Gemini's: the rules file carries the
  static tiers per turn, the `PreInvocation` `userMessage` carries the live surface
  once.
- **Gemini and Antigravity context files now carry the live surface.** Both hosts
  re-read their context file every prompt/turn, so Catenary rewrites its own
  installed artifact at hook time — the file becomes the compaction-proof delivery
  channel, not just a static bootstrap. A Gemini `SessionStart` regenerates the
  extension's gemini-context.md; an Antigravity
  `PreInvocation` regenerates the plugin's
  [rules/catenary.md](plugins/catenary-antigravity/rules/catenary.md). Each
  rewrite carries the workspace-invariant live
  surface (the header, the user-global commands surface —
  allow/pipeline/deny/forms/script-hosts + write-model line — the invariants, and
  the flag synopses; per-session data like the cwd build tool stays on the
  injection channels). Writes are hash-gated and atomic (render → compare →
  temp-file + rename only on a change), so the per-model-call path is a cheap
  no-op, and fail-open (any error is swallowed so the hook's injection is never
  blocked). A link-install guard skips the rewrite when the install resolves into a
  git worktree (the dev-repo symlink case), so a development checkout is never
  dirtied. The shipped files stay the cold bootstrap; the runtime-rewritten copy
  carries an invisible generation stamp so `catenary doctor` accepts it as current.
  The `SessionStart` `additionalContext` and `PreInvocation` `userMessage`
  injection channels are unchanged.
- **Gemini teaching goes hook-only; its context file is retired.** Source-reading
  the installed Gemini CLI (0.46.0) falsified the ticket-12 premise: context files
  are read once and cached (refresh only at startup / `/memory reload` / MCP
  refresh / trust change — never per prompt), so hook-time rewrites never reached a
  live session. The extension `contextFileName` and the shipped gemini-context.md
  are dropped, along with the runtime regeneration and its freshness/doctor checks.
  Gemini teaching now rides hooks exclusively: `SessionStart` re-stamps the full
  payload (`startup`/`clear`), and two new hooks close the compaction gap —
  `PreCompress` (`catenary hook pre-compress --format=gemini`) lays a per-session
  discontinuity mark under `runtime_dir` (no daemon), and `BeforeAgent` (`catenary
  hook before-agent --format=gemini`) consumes a pending mark to re-inject the
  payload once via `hookSpecificOutput.additionalContext`. **Firing semantics,
  bundle-pinned:** `PreCompress` fires *before* the token-threshold check and
  `compress()` runs every turn on the default legacy path, so `PreCompress` fires
  on every turn including below-threshold no-ops; its only field, `trigger`, cannot
  tell a real auto-compaction from a no-op. The mark is therefore laid only on
  `trigger: manual` (a user's `/compress`, which sets `force = true` and bypasses
  the threshold — a provable compaction), and consumed at most once, so the payload
  never re-injects per turn. Documented residual: an *auto*-compaction re-injects
  nothing (it is indistinguishable from the every-turn no-op) — the accepted gap,
  strictly preferred over per-turn injection.
- **Antigravity's `PreInvocation` first-sighting shrinks to the per-session
  sliver.** The always-on rules file already carries the workspace-invariant
  surface every turn, so the once-per-conversation `PreInvocation` injection no
  longer duplicates the full payload — it carries only the session-specific delta
  the rules file structurally cannot (`teaching::session_sliver`): the cwd build
  tool, as a self-labeled block. A test pins that the sliver and the rules-file body
  share no line. When the cwd resolves no build tool there is no delta and nothing
  is injected. The `context_files` header comment is corrected to the evidence: the
  Gemini per-turn re-read claim was false (machinery now deleted); the Antigravity
  cadence is unconfirmed, and the rewrite is correct either way — live if re-read
  per turn, delivered by the next conversation start if cached.
- **The teaching primer speaks an informative capability voice, not policy
  assertion.** The primer / SessionStart / SubagentStart payload and the runtime
  context files no longer assert that navigation "stays denied": the write-model
  line's navigation half now names `catenary grep` and `catenary glob` as the
  navigation tools — config-free and always present, replacing the teach-07
  clause that named which native scanners the live config denied (a user may
  enable those scanners, so the primer states capabilities, not policy). The
  "Navigate through Catenary" invariant is reworded to the same voice: enrichment
  rides along where a code intelligence source covers a hit, and `catenary grep`
  reads stdin (`… | catenary grep PAT` — a plain pass, complete matches, no
  enrichment) so matches always come back even where no source covers them. The
  live allow / pipeline / deny lists — the session's actual policy — are
  untouched; the shipped gemini-context.md and Antigravity rules regenerate from
  `fallback_body()` under the same freshness gates.
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
- **Server window messages are firehose-only — the notification queue is
  reserved for Catenary's own events.** LSP-server-forwarded `window/logMessage`
  **and** `window/showMessage` notifications no longer reach the user
  notification queue at any severity; they are recorded to the JSONL firehose
  (queryable, TUI-visible) only. Previously a server's `window/showMessage`
  type 1 (Error) cleared the `[notifications] threshold` and surfaced as a user
  notification — the noise even crossed sessions, promoting one session's
  transient per-server chatter (e.g. rust-analyzer's "Failed to load workspaces"
  under load) into another session's stream. Both forwarded window methods are
  now tagged `source = lsp.logging` at the forwarding site, and the notification
  sinks exclude that origin. A genuinely broken server still surfaces where it
  matters (the `unavailable:` banner on the diagnostics receipt). The
  `[notifications] threshold` semantics are unchanged for Catenary's own events.
- **`make check` clippy runs `--all-features`.** The lint pass now runs
  `cargo clippy --tests --all-features -- -D warnings` instead of
  `--features mockls`, so the CI gate lints the full feature surface — including
  the `fuzzing`-gated differential oracle in its non-test compilation — exactly
  as the user's rust-analyzer flycheck does under `cargo.features = "all"`. This
  closes the clippy-surface divergence where a `catenary diagnostics` receipt
  could flag a lint the gate never saw (feedback 09). No source changes were
  needed: the widened surface flagged zero new warnings.

### Fixed

- **`catenary glob --exclude-pattern` now excludes pattern matches.** The
  compiled exclude reached only named-directory walks, so files matched by a
  glob pattern argument leaked past it — in both the listing and `--count`
  (`'**/*' --include-hidden --exclude-pattern '**/.git/**'` returned the
  `.git` contents anyway). Pattern matches, directory entries, `--count`,
  and the LSP nudge now filter identically, and a pattern whose every match
  is excluded reports the honest no-matches line instead of vanishing.
  `catenary grep --exclude-pattern` was unaffected and is now pinned by a
  regression guard.
- **The stale-hooks notification names the real command.** The daemon-startup
  staleness check said `Run: catenary install` — a subcommand that only lists
  detected hosts. It now names the exact per-host command
  (`catenary install claude` / `catenary install antigravity`), matching the
  doctor surface's wording. Relatedly, the daemon-side docs claiming "the CLI
  resolves every path argument against cwd" are trued — the *daemon*
  absolutizes relative search paths against the request's cwd (`to_params`,
  since misc 102) — and three regression tests now pin cwd-anchoring for
  relative `grep`/`glob` patterns from subdirectories and nested checkouts.
- **`<>` (read-write) redirects lex as one operator.** The faithful shell
  parser had no read-write redirect: `<>` split into a `<` then a `>` — two
  redirects where bash performs one — diverging from the brush oracle (found
  by the differential fuzz soak). `<>` and `N<>` now lex as a single
  `ReadWrite` operator projected to the output-file class: it opens its
  target writable, so the redirect guard and write resolver gate it as a
  write (edit-visibility preserved; the deny outcome is unchanged — only the
  operator count and a spurious input-file projection were wrong). The
  minimized case is graduated into the differential corpus with
  assert-on-value pins for the tokenization, the oracle projection, and the
  write resolution.
- **Top-level `catenary --version`/`--help` admit only as sole flags.** The
  canonical-form matcher admits the global informational flags (`--version`/
  `-V`/`--help`/`-h`) only when the flag is the entire argument vector —
  `catenary --version extra-arg` and paired flags now fall through to the
  fail-closed denial instead of being wrongly admitted. The `version`
  subcommand (which also reports the running daemon's version) keeps its
  normal lenient admission.
- **A failing `cd` before a `;` no longer mis-attributes a later relative
  write.** The write resolver simulates `cd` by threading the target through
  the effective cwd, but couldn't tell a `cd` would fail at run time. For
  `cd missing; printf x > f` the shell writes `./f` (the `cd` fails, execution
  continues in the old cwd) while the resolver recorded `missing/f` — a path
  nobody wrote, missing the one that was. The resolver now existence-checks the
  `cd` target: a directory that neither exists at resolve time nor is provably
  created by an earlier `mkdir` in the same command line fails toward the
  existing cwd-poison, so a later *relative* target becomes an honest denial
  instead of a mis-attributed record. Separator semantics are preserved:
  after `&&` a failed `cd` short-circuits everything after it, so the intended
  path is still recorded (pinned unchanged); the poison is applied only where a
  `;`/`||` continuation would diverge. Absolute targets after a failing `cd`
  stay resolvable — the cwd is irrelevant to them (bug 63).
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
- **`catenary grep` no longer silently skips a searchable file.** A file over
  the 10 MB binary-scan cap is classified binary *without being read* (the size
  heuristic in `scan_file`), so the grep walk dropped it with no trace — an
  explicitly named 15.7 MB UTF-8 bundle returned `0 matches in 0 files`,
  indistinguishable from a genuine no-match (the absence of evidence read as
  evidence of absence). A skipped file is now reported, never silent: the
  default output appends `skipped (<reason>): <path>` for every explicitly named
  file (positional arg or a glob that expanded to it), directory-walk skips of
  unnamed files collapse to `<n> file(s) skipped (<reason>)`, and `--count`
  carries a `(<k> skipped: <reason>)` suffix so a skip is never conflated with a
  no-match (e.g. `0 matches in 0 files (1 skipped: too large (>10 MB))`). The
  reason distinguishes the size cap (`too large (>10 MB)`) from genuine NUL-byte
  content (`binary`). A search with nothing skipped renders byte-identically to
  before, and the exit stays `0` on this soft condition.

### Removed

- **Gemini CLI support is withdrawn** (decision 030). Google's June 18, 2026
  cutoff of individual, free, and Pro-tier Gemini CLI access left the host
  untestable by individuals, and Catenary is verified by live dogfooding — an
  unrunnable host can only ship faith-based teaching. The maintainer also
  declines to donate support labor to an ecosystem that accepted a year of
  community pull requests against an Apache-2.0 codebase and then locked those
  contributors out behind a proprietary, paywalled backend. `catenary install
  gemini` now prints a one-line withdrawal note and installs nothing (remove any
  prior extension by hand: `gemini extensions uninstall catenary`, or delete
  `~/.gemini/extensions/catenary`); the `pre-compress` / `before-agent` hooks,
  the Gemini extension manifest, and the Gemini doctor checks are gone.
  Antigravity CLI support continues — it is the Google-ecosystem path
  individuals can still run.

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
