# CLI & Dashboard

## Dashboard (TUI)

Running `catenary` in an interactive terminal launches the TUI dashboard.
When stdin and stdout are pipes (launched by an MCP client), it serves
MCP instead — no flags needed.

The dashboard is the primary way to answer *"is it working?"* at a
glance. It reads a daemon-owned `state.json` snapshot plus the health
model's findings and renders a **2×2 master-detail grid**: the
**Servers (by root)** tree (top-left, grouped by root, healthy fleets
collapsed to one line each), the **Sessions (by client)** tree
(bottom-left, grouped by client with capability-aware session status),
a contextual **Details (Servers / Sessions)** pane (top-right — titled
for the focused tree: config / routing / findings with provenance /
session actions for the cursored node), and the **problems pane**
(bottom-right — the durable notification surface, every finding with its
fix-it). There is no header strip: the Problems pane title carries the
one-line verdict (`● working` / `✗ N problems · M suggestions`), and the
footer carries the daemon pid, version + skew, and snapshot freshness. It
is a pure file reader — it never connects to the daemon, probes an LSP, or
opens the firehose. Full protocol and trace history streams to an
append-only JSONL telemetry firehose, which `catenary query` reads after
the fact.

```bash
catenary  # launch dashboard
```

### Keybindings

Navigation is keyboard-first; mouse click is an equal path (click a pane
to focus it, click a row to select/expand, click a problem to jump to
its owner):

| Key | Action |
|-----|--------|
| `j` / `Down` | Move down one entry |
| `k` / `Up` | Move up one entry |
| `Tab` | Focus the next pane |
| `Shift+Tab` | Focus the previous pane |
| `Enter` | Expand/collapse a node, or focus a problem's owner |
| `p` | Problems-only — collapse both trees to broken things |
| `d` | Toggle the dormant-server inventory |
| `g` / `Home` | Jump to the first entry |
| `G` / `End` | Jump to the last entry |
| `PageDown` / `PageUp` | Page down / up |
| `y` | Yank the selected entry (scope id / text) via OSC 52 |
| `?` | Toggle the keybinds help panel |
| `q` | Quit |

## Protocol Transparency

Catenary logs every protocol message — every MCP exchange, every LSP
request and response, every hook invocation — to an append-only JSONL
telemetry firehose, sharded per session, server, and tool invocation:
what Catenary sends to your language servers, what they send back, and
how long each exchange takes. `catenary query` reads it after the fact;
the TUI renders a live snapshot of the resulting state.

You can see exactly what Catenary does. Nothing is hidden.

## CLI Commands

### `catenary grep`

Search for a pattern across the workspace. Queries ripgrep and the LSP
symbol index in parallel. Results are LSP-enriched within tracked
workspace roots. Uses the shell's current working directory as the
search root.

```bash
catenary grep "pattern"
catenary grep "foo|bar" "src/**/*.rs"
catenary grep "TODO" --type rust        # restrict to a ripgrep file type
catenary grep "fn main" --glob "src/**" # scope which files are searched
catenary grep "TODO" --exclude-pattern "vendor/**"
catenary grep "pattern" --include-hidden --include-gitignored
catenary grep "TODO" --count            # "N matches in M files"
```

Quote glob patterns so Catenary expands them gitignore-aware rather than
the shell. Output is complete every time — no truncation, paging, or spill
files — and composes freely with pipes and redirects (`catenary grep p |
head` works). Ask for a total with `--count`, narrow with `--type` or
`--glob`, and exclude with `--exclude-pattern`.

| Flag | Description |
|------|-------------|
| `[PATH]...` | File or directory path(s) to scope the search (quoted globs allowed) |
| `--glob <pat>` / `-g` | Include only files matching this glob (repeatable; `!pat` excludes) |
| `--type <ty>` / `-t` | Include only files of this ripgrep type, e.g. `rust`, `md` (repeatable) |
| `--exclude-pattern <pat>` | Glob pattern to exclude from matches |
| `--ignore-case` / `-i` | Case-insensitive matching (overrides smart-case) |
| `--case-sensitive` / `-s` | Case-sensitive matching (overrides smart-case) |
| `--word-regexp` / `-w` | Match whole words only |
| `--fixed-strings` / `-F` | Treat the pattern as a literal string, not a regex |
| `--invert-match` / `-v` | Select non-matching lines |
| `--files-with-matches` / `-l` | Print only the paths of files containing a match |
| `--after-context <n>` / `-A` | Show `n` lines of context after each match |
| `--before-context <n>` / `-B` | Show `n` lines of context before each match |
| `--context <n>` / `-C` | Show `n` lines of context before and after each match |
| `--count` / `-c` | Report the match count instead of results |
| `--include-gitignored` | Include files ignored by .gitignore |
| `--include-hidden` | Include hidden files and directories |

### `catenary glob`

Browse the workspace: file outline, directory listing, or glob pattern
match. Auto-detects intent from each PATH — a file path shows a symbol
outline, a directory path shows a listing with symbols, and a glob
pattern shows matching files.

A PATH may be a **glob pattern**: quote it so the shell doesn't expand it
and Catenary walks it gitignore-aware instead. Patterns may be absolute or
cwd-relative, and the anchor belongs *in the pattern* — there is no
separate directory argument (`catenary glob 'src/**/*.rs'`,
`catenary glob '/abs/dir/**/*.md'`). Each pattern argument's results open
with a one-line cardinality header — `N files match <pattern>` (singular
grammar for one) — printed *before* the per-file listings, so a
`| head`-truncated view still shows the true count. A pattern that expands
to nothing is never silent either: it reports `no matches for pattern:
<pattern> (relative patterns anchor at cwd)`, per argument, even when
sibling arguments render. (Directory and single-file arguments render
unchanged — a directory shows its own structure, a named file is its own
answer.)

The outline is a **map, not a mirror** — it renders **types and callables
only**. It recurses into containers (modules/namespaces/packages and
classes/interfaces/enums/structs/impls), showing the containers and their
functions, methods, and constructors. Data members (fields, properties,
enum variants, and variables/constants below the top level) are pruned, and
a callable's interior (locals, loop vars, nested defs) is never entered —
each callable is one line. The top level shows everything, so a
module-level constant stays. (The underlying symbol index is unfiltered;
only the outline render applies this map.)

```bash
catenary glob "src/"
catenary glob "src/main.rs"
catenary glob "**/*.toml"                 # opens "N files match **/*.toml"
catenary glob "**/*.rs" --exclude-pattern "tests/**"
catenary glob "**/*.rs" | head -3        # header shows the true count first
catenary glob "**/*.rs" --count          # "N paths"
```

Like `catenary grep`, glob emits complete output — pipe or redirect it
freely — and `--count` answers "how many" without the listing.

| Flag | Description |
|------|-------------|
| `[PATH]...` | File, directory, or quoted glob pattern(s) — absolute or cwd-relative, anchor in the pattern |
| `--exclude-pattern <pat>` | Glob pattern to exclude from results |
| `--count` | Report the path count instead of results |
| `--include-gitignored` | Include files ignored by .gitignore |
| `--include-hidden` | Include hidden files and directories |

### `catenary diagnostics`

Print LSP diagnostics for the files you've edited, or lint the paths you
name. Editing is tracked automatically — the first edit to a
server-covered file starts it, there is no start step. Bare, this command
diagnoses the current **batch**: it opens every modified file on its server,
waits for each to settle, and prints a **per-file receipt** — every
diagnosed file listed, its errors and warnings beneath it, or `[clean]`
beside it when the file is clean. The batch is durable, not consumed: run
bare again with no intervening edit and it re-diagnoses the same set, fresh
(the `git status` idiom). Your next covered edit after a fully-diagnosed
batch starts a new one. When a file's
server dies before answering — mid-run, or by failing to start at all —
Catenary makes **one bounded, in-run attempt** to respawn it and re-run the
remainder (a slight stall, never an unbounded wait); if that fails, coverage
has **degraded**. Coverage is *effective, not nominal*: a server that cannot
be brought back means its files owe nothing for this run — the same class as
a file no server covers, because the gap is Catenary's to close, never
yours. Such a file is neither clean nor dirty; it is listed as `[unverified
— <server> returned no result]` — or `[unverified — <server> stuck]` when
process-state evidence types the server as wedged (respawn-dead, or init-hung so
its tick-budgeted `initialize` failed): "stuck" is a claim about the process,
made only on the evidence, so an armed gate is **always payable** — a stuck
server yields an honest receipt rather than a silent hang, and paying is
diagnosing, not fixing. The receipt **opens with a top-line
banner naming the unavailable server** (`unavailable: <server>`) so degraded
never reads as clean — the absence of evidence is not evidence of absence.
An all-unverified run can never render as empty stdout (mistakable for a
hang), and the exit stays `0`: the run completed and its receipt is
truthful. The gate releases the degraded file exactly as a paid one — editing
it again re-arms it, and a server that is back next run resumes the normal
contract. When nothing was edited it prints `[no edited files]`.

**The batch survives a killed client.** A `catenary diagnostics` run pays its
debt by *delivery*, not at dispatch: the batch's per-file flags flip only after
the daemon's response reaches the CLI. So an invocation killed after dispatch (a
backgrounded command reaped by the host, a tool-call timeout, a Ctrl-C) leaves
the flags unflipped and the gate armed — the batch is intact, and the next bare
run re-diagnoses it in full. A kill *after* a successful write recovers the same
way: the batch is retained, so re-running bare re-serves it. Recovery is always
"run it again."

**The batch does not survive the daemon.** It is in-memory state keyed by
`(session_id, agent_id)`: durable *across runs within a daemon instance*, but
**released when the instance dies** (maintainer ruling, bug 79). On daemon death
the debt is dropped, never spooled — a fresh daemon starts with a disarmed gate,
and a bare run answers `[no edited files]`. This is deliberate: an unstable
daemon must never lock a session out of the shell. The cost is that unpaid debt
across a restart is forgotten silently; the benefit is that a wedged daemon is
always recoverable by restart, never a permanent lockout.

**With and without hooks.** The batch is populated by the `PreToolUse` hook,
which tracks every file the agent edits. In a **hooked** session (the plugin
installed) `catenary diagnostics` behaves exactly as above — the bare form pays
the tracked batch, and a scoped form pays the named files' debt. On a
**hookless** box (no plugin, e.g. a scripted or CI invocation, or a bare shell)
there is no tracked batch, so the two forms split:

- **Bare** `catenary diagnostics` is the gate verb, and there is no gate to pay:
  it **errors** with a teaching message and exits `2` (a fault, not a clean
  empty receipt). Naming what you want diagnosed is the fix.
- **Scoped** `catenary diagnostics <path…>` — including `catenary diagnostics .`
  — has no debt to settle, so it simply **serves the diagnostics on demand**:
  it diagnoses the named paths (mounting an enclosing project root ephemerally
  when needed) and prints the receipt, with no gate machinery. This is the
  CLI-only lint surface — `doctor` → `pin` → `diagnostics .` works with no host
  plugin at all.

```bash
catenary diagnostics                 # the whole edited set (hooked)
catenary diagnostics src/main.rs     # lint one file on demand
catenary diagnostics src/ lib.rs     # a scoped set (relative to cwd)
catenary diagnostics .               # the whole workspace root
```

**Whole-root scope (`.`).** Naming a directory lints every covered file
beneath it; naming a whole tracked workspace root (`.`) lints the entire
project. When the covering language server advertises whole-workspace pull
(`workspace/diagnostic`), Catenary serves `.` with **one** request off the
server's existing project model — no per-file open/close churn, and it
surfaces cross-file diagnostics a per-file pull can miss. A server without
that capability, or any *sub-root* directory, falls back to the per-file
pass (identical results, more work). Because a whole-root run can span many
files, the receipt **collapses the clean files to a count** (`N files
clean`) — and likewise any unverified files (`M files unverified`) — and
lists only the files that have diagnostics — the complete diagnostics still
print in full; only the clean and unverified lists are folded. The
edit-loop receipt (a handful of files) stays per-file, with `[clean]` or the
`[unverified — …]` line beside each.

**The edit gate is a debt paid by *diagnosing*, not fixing.** Every
server-covered file you edit joins the batch; each file's debt is paid
by *looking at* it — pulling its diagnostics, clean or dirty — after which
you choose whether to fix. Bare pays the whole batch at once (it diagnoses
every file, delivered or not, so a later edit's cross-file effects surface).
Naming paths pays exactly those: a **partial** pull leaves the gate **armed**
for the files you didn't name, so the command filter keeps blocking unrelated
commands until the rest are diagnosed. Editing a
paid file re-arms it. A named path that was never edited is simply linted
on demand — it pays nothing, since it owed nothing. Relative paths resolve
against the shell's current working directory. A named path that does not
exist, or that resolves **outside every mounted root**, is never dropped in
silence. When the path has a detectable enclosing project root (walking
`.git` up from it), Catenary **mounts that root ephemerally** and diagnoses
the file from the freshly-attached server — the mount then expires after a
few minutes of inactivity (or `catenary pin` pins it). When no
enclosing root is detectable, the receipt names the path on its own line and
says why (`path does not exist`, or that it is outside every mounted root).

`catenary diagnostics` is a load-bearing command — run it (bare or scoped)
as its **own step** (no pipes, no `&&`/`;` chaining), and read the result.
**The exit code is a trust signal, not a lint result:** it exits `0`
whenever the run completed — clean *or* dirty — and `2` only on a genuine
fault (no daemon, IPC failure, or a bare hookless run with no gate to pay).
It never exits `1`, so a run that found errors is not mistaken for a failed
call — read the receipt for the errors, not the exit code. (Whether a run is
labeled "dirty" is tunable via `diagnostics_severity` in
[Configuration](configuration.md#diagnostics), but that is a status label only
and does not change the exit code.)

### `catenary query`

Query the JSONL telemetry firehose — every LSP, MCP, and hook message,
plus internal trace events. Reads the append-only logs directly, so it
works even when the daemon is down. Useful for debugging and bug reports.

Filters fall into two groups. *File-selection* filters pick which shards
to read: `--session` (one session's log), `--server` (an LSP server),
`--tool` (a `grep`/`glob` invocation). *Record* filters apply after open:
`--cwd`, `--since`, `--level`, `--kind`, and `--search`.

```bash
catenary query --session 029ba740 --since 1h
catenary query --kind hook --since today
catenary query --search "timeout" --format json
catenary query --server rust-analyzer --level warn --follow
```

| Flag | Description |
|------|-------------|
| `--session <id>` | Read one session's log (id or prefix) |
| `--server <name>` | Read an LSP server's log (all instances) |
| `--tool <grep\|glob>` | Read a search tool's invocation log |
| `--cwd <path>` | Keep records whose cwd is this path or under it |
| `--since <dur>` | Time filter (`1h`, `today`, `7d`, `30m`) |
| `--level <lvl>` | Minimum severity (`error`/`warn`/`info`/`debug`) |
| `--kind <kind>` | Record kind (`lsp`/`mcp`/`hook`/`internal`) |
| `--search <text>` | Free-text substring over method, message, payload |
| `--instance <id>` | Read a specific daemon instance dir (default: freshest) |
| `--all-instances` | Read every instance dir, not just the freshest |
| `--follow` | Live-tail the selected files |
| `--limit <n>` | Max rows (0 = unlimited; default 100) |
| `--format <fmt>` | Output format: `table` (default) or `json` |

### `catenary pin` / `catenary unpin` / `catenary roots`

Manage workspace-root **lifetime**. Coverage is automatic — Catenary mounts and
serves the workspace for you — so these change only how long a root lives, not
whether it is served.

```bash
catenary pin /path/to/project     # stop idle expiry, pre-warm servers, upgrade an ephemeral mount
catenary unpin /path/to/project   # drop the pin added by `catenary pin`
catenary roots                    # list the current roots with their contributor classes
```

`catenary pin` adds the pin contributor and pre-warms the root's language
servers; on an activity-mounted (ephemeral) root it upgrades the mount to pinned
so it stops expiring. `catenary unpin` removes **only** the pin contributor,
matching the stored/normalized path — so it works even after the directory has
been deleted, and repeating it is a harmless no-op. The worktree, ephemeral, and
`mcp:` contributor classes own their own lifecycles and are untouched. Bare
`catenary roots` lists the current roots (`catenary roots ls` is a kept alias).

The old `catenary roots add` / `catenary roots rm` spellings are retired: use
`catenary pin` / `catenary unpin`.

### `catenary worktree`

Manage Catenary-created worktrees — the sanctioned replacement for `git
worktree` (which the agent surface denies). A worktree is a durable, isolated
checkout of a branch that language servers index like any other root, so you can
prepare a change in isolation and land it when it is ready.

```bash
catenary worktree add my-feature          # create a feats-class worktree for a branch
catenary worktree ls                      # list Catenary-managed worktrees
catenary worktree diff <path>             # print the worktree's full diff vs HEAD
catenary worktree land <path>             # apply + stage the changes, then retire the worktree
catenary worktree rm <path>               # remove a worktree
```

| Subcommand | Description |
|------------|-------------|
| `add <branch> [path]` | Create a durable worktree for `<branch>` (default path under Catenary's state dir; pass an explicit `path` to override). Adds a sibling symlink for discovery. |
| `ls` | List Catenary-managed worktrees — path, class, creator, age, clean/dirty, and (for `feats` worktrees) ahead/behind counts. |
| `diff <path>` | Print the worktree's complete diff vs `HEAD` — tracked changes plus untracked files as new-file hunks — as a valid `git apply` patch. `--name-only` prints just the changed paths. |
| `land <path>` | Apply the worktree's diff into the owning repo with `git apply --3way`, **stage** the result, arm a diagnostics batch over the changed files, delete the branch, and retire the root. It **never commits** — you review and commit. `--keep` lands without removing the worktree. |
| `rm <path>` | Remove a worktree class-appropriately. A dirty worktree is never auto-reaped — `rm` refuses to discard uncaptured work. |

`land` stages but does not commit, so the changes land in your index for review.
A dirty worktree is never removed automatically: unlanded work is always kept
until you land or explicitly remove it.

### `catenary doctor`

Verify language servers and hook installation. See [Installation](installation.md#verify).

Pass a server name for verbose single-server diagnostics:

```sh
catenary doctor rust-analyzer
```

Verbose mode prints the resolved command, binary path, stderr capture,
full `initialize` request/response JSON, and capabilities list.

| Flag | Description |
|------|-------------|
| `[server]` | Server name for verbose single-server mode (matches `[lsp.server.*]` keys) |
| `--root <path>` | Workspace root to probe and read `.catenary.toml` from (default: cwd) |
| `--diff` | Show a unified diff for every stale host file (`hooks.json`, the constrained-bash helper) |
| `--nocolor` | Disable colored output |

### `catenary start` / `catenary stop`

`catenary start` brings the daemon up explicitly — the counterpart to `stop`.
It is idempotent: if a daemon is already running it connects, reports that, and
leaves it running. You rarely need it, because the bridge starts (and
transparently reconnects) the daemon on demand; it exists so a manual `stop` or
a killed daemon has a one-command remedy without a per-session `/mcp` reconnect.

```sh
catenary start   # bring the daemon up (idempotent)
```

`catenary stop` stops the running daemon. When you run it in an interactive terminal and
sessions are still connected, it prints the session board first — each
connected session's host, workspace root(s), and how long it has been
connected (read from the `state.json` snapshot) — and asks for confirmation
before disconnecting anyone. Declining (the default) exits `0` with the
daemon left running.

```sh
catenary stop            # confirm before disconnecting live sessions
catenary stop --force    # skip the prompt (scripts, upgrade flow)
```

`--force` skips the prompt, and a non-interactive stdin skips it too, so
scripts and the documented upgrade flow are unaffected. After the stop, a
warning names how many sessions lost tooling — each needs a `/mcp` reconnect,
since a host restart alone won't respawn the daemon.

| Flag | Description |
|------|-------------|
| `--force` | Stop without the confirmation prompt, even with live sessions |

### `catenary version`

Print the CLI version **and** the running daemon's version. `catenary
--version` (the clap flag) prints only the binary's own version instantly, with
no daemon I/O; the `version` subcommand additionally queries the daemon, so it
surfaces version skew — a daemon lags a freshly rebuilt CLI until it is
restarted, and this shows that at a glance.

```sh
catenary --version   # this binary only (instant)
catenary version     # this binary + the running daemon
```

### `catenary update`

Self-update the `catenary` binary from the latest GitHub release for your
platform (`catenary-linux-amd64`, `catenary-macos-arm64`,
`catenary-windows-amd64`). There is no Intel-mac asset — build from source on
Intel hardware.

```sh
catenary update           # download and replace the binary if newer
catenary update --check   # report whether an update is available, download nothing
catenary update --force   # re-download even when versions already match
```

| Flag | Description |
|------|-------------|
| `--check` | Print whether an update is available without downloading |
| `--force` | Re-download even if the installed version already matches |
