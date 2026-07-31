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
| `a` | Open the cursored row's guided action — on a vetted install suggestion, the [guided-install](#guided-install) consent overlay |
| `g` / `Home` | Jump to the first entry |
| `G` / `End` | Jump to the last entry |
| `PageDown` / `PageUp` | Page down / up |
| `y` | Yank the selected entry (scope id / text) via OSC 52 |
| `?` | Toggle the keybinds help panel |
| `q` | Quit |

### Guided install

A missing server that an active language routes to surfaces in the
problems pane as an install *suggestion* — advisory, never counted as a
problem. When the server is in the conformance-vetted set, the
suggestion's fix-it shows the exact pinned install command, and pressing
`a` on the row opens a consent overlay previewing the resolved install
plan — nothing runs until you confirm. `Enter` executes the plan through
Catenary's verified install engine (the same one `auto_install` uses),
landing the pinned version in the managed home; the outcome log replaces
the preview, naming any stale managed versions collected, and on success
the suggestion clears. `Esc` dismisses at any point. A recipe whose
artifact cannot be verified (an npm/pip package carrying no hash to check)
is refused — the overlay says so instead of offering to run it. See
[Managed Server Installs](configuration.md#managed-server-installs).

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

**stdout carries results only** (the VERBS streams ruling). A search with no
matches prints nothing on stdout (empty set, exit 0) and reports the
`no matches for: <pattern>` echo plus its `searched:` scope on **stderr**; a
skipped file (binary) and a missing named path likewise ride stderr. An
**invalid pattern is a usage error** — the parse error prints on stderr and
the command exits 2 (on the bare *and* `--count` forms, ripgrep parity),
never a zero indistinguishable from a genuine no-match.

| Flag | Description |
|------|-------------|
| `[PATH]...` | File or directory path(s) to scope the search (quoted globs allowed) |
| `--glob <pat>` / `-g` | Include only files matching this glob (repeatable; `!pat` excludes) |
| `--type <ty>` / `-t` | Include only files of this ripgrep type, e.g. `rust`, `md` (repeatable) |
| `--exclude-pattern <pat>` | Glob pattern to exclude from matches (repeatable; a path is dropped when any matches) |
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

Browse the workspace by **glob pattern**. The single positional is a
pattern, decoded syntactically, always — quote it so Catenary expands it
gitignore-aware, not the shell (`catenary glob 'src/**/*.rs'`). A
metachar-free spelling is a **self-matching literal**: `catenary glob
src/main.rs` outlines that file, `catenary glob 'src/*'` lists the
directory. Patterns may be absolute or cwd-relative, and the anchor belongs
*in the pattern* (`catenary glob '/abs/dir/**/*.md'`); there is no separate
directory argument.

**Exactly one pattern.** Multiple patterns are a brace alternation
(`catenary glob '{src,tests}/**/*.rs'`); a bare `glob`, or an unquoted
pattern the shell expanded to several words, is a usage error with teaching
(stderr, exit 2). Gitignore semantics are uniform regardless of shape:
`--include-gitignored` is the one lever, `--include-hidden` relaxes wildcard
traversal.

**stdout carries results only** (the VERBS streams ruling). There is no
cardinality header — `--count` is the sole tally. A pattern that expands to
nothing prints nothing on stdout (empty set, exit 0) and reports `no matches
for pattern: <pattern> (relative patterns anchor at cwd)` on **stderr**,
followed by a raw-string disclosure when the target exists but is
gitignored/hidden. Teaching — the metachar-bearing matched-name note and the
directory note (`<dir>` is summarized above; `catenary glob '<dir>/*'` matches
its entries as first-class, individually-enriched paths) — rides stderr too;
an explicit `2>/dev/null` is consent to lose it.

The outline is a **map, not a mirror** — it renders **types and callables
only**. It recurses into containers (modules/namespaces/packages and
classes/interfaces/enums/structs/impls), showing the containers and their
functions, methods, and constructors. Data members (fields, properties,
enum variants, and variables/constants below the top level) are pruned, and
a callable's interior (locals, loop vars, nested defs) is never entered —
each callable is one line. The top level shows everything, so a
module-level constant stays. (The underlying symbol index is unfiltered;
only the outline render applies this map.)

**Listings default to top-level structure.** A listing shape — a matched
directory, or several matched files — annotates each file with its
top-level symbols only, no nested tree; `--outline` opts up to the full
tree on demand. A single matched file (`catenary glob src/main.rs`) is the
file-outline shape and always gets the full tree. There is deliberately no
enrichment-off flag: `--count` covers tallies, and a pipeline that needs
bare paths strips the indented enrichment downstream.

```bash
catenary glob "src/*"                     # list a directory (top-level structure)
catenary glob "src/*" --outline           # the same listing, full outlines
catenary glob src/main.rs                  # outline one file (self-matching literal)
catenary glob "**/*.toml"                  # results only — no header
catenary glob "**/*.rs" --exclude-pattern "tests/**"
catenary glob "**/*.rs" | head -3         # stdout is pure results — safe to pipe
catenary glob "**/*.rs" --count           # "N paths" — the sole tally
catenary glob "{src,tests}/**/*.rs"        # multiple patterns: brace alternation
```

Like `catenary grep`, glob emits complete output — pipe or redirect it
freely — and `--count` answers "how many" without the listing.

| Flag | Description |
|------|-------------|
| `PATH` | A single glob pattern (quoted) — absolute or cwd-relative, anchor in the pattern; a metachar-free spelling is a self-matching literal |
| `--exclude-pattern <pat>` | Glob pattern to exclude (repeatable; a path is dropped when any matches). Reaches a matched directory's listing too: an excluded entry neither renders nor counts in the directory's file/dir tally |
| `--count` | Report the path count instead of results (the sole tally) |
| `--include-gitignored` | Include files ignored by .gitignore |
| `--include-hidden` | Include hidden files and directories |
| `--outline` | Full symbol outlines in listings (default: top-level structure; a single matched file is always full) |

### `catenary diagnostics`

Print LSP diagnostics for the files you've edited, or lint the paths you
name. Editing is tracked automatically — the first edit to a
server-covered file starts it, there is no start step. Bare, this command
diagnoses the current **ledger** — the durable, on-disk set of files edited
since their last diagnosis, kept per workspace root (the "kitchen"): it opens
every due file on its server, waits for each to settle, and prints a
**per-file receipt** — every diagnosed file listed, its errors and warnings
beneath it, or `[clean]` beside it when the file is clean. Edits book into
each file's *own* root, so one session's debt can span kitchens; the bare run
serves **every kitchen holding your debt** — the cwd's root plus your other
indebted roots — with the receipt grouped per root, so debt one kitchen over
is never invisible from where you stand. Diagnosing a file
pays it off: it leaves the ledger. So once you have run bare and served the
edited set, the debt is paid — a repeat bare run with no intervening edit
finds an empty ledger and answers `[no edited files]`. To re-check specific
files, name them with the scoped form. Your next covered edit re-arms the
ledger. When a file's
server dies before answering — mid-run, or by failing to start at all —
Catenary makes **one bounded, in-run attempt** to respawn it and re-run the
remainder (a slight stall, never an unbounded wait); if that fails, coverage
has **degraded** for this run. A dead server is not abandoned: the next
demand that routes to it (a diagnose or query) **revives it**, bounded by a
per-server **strike counter** — each failure (a crash while up, a failed
respawn, a failed `initialize`) is a strike, each served result pays one
back, and at three strikes the server is **benched**: no further revives
until the daemon restarts or the root is remounted, so a crash-looping
server never flaps unbounded. Coverage is *effective, not nominal*: a server
that cannot be brought back means its files owe nothing for this run — the
same class as a file no server covers, because the gap is Catenary's to
close, never yours. Such a file is neither clean nor dirty; it is listed as
`[unverified — <server> returned no result]` — or `[unverified — <server>
stuck; will retry on demand]` when process-state evidence types the server
as wedged (respawn-dead, or init-hung so its tick-budgeted `initialize`
failed): "stuck" is a claim about the process, made only on the evidence. A
benched server's files carry the terminal cause instead: `[broken — <server>
never started]` (it struck out without ever serving — config or environment;
fix the server) or `[unstable — <server> gave up after repeated crashes]`.
Every state pays: an armed gate is **always payable** — a stuck or benched
server yields an honest receipt rather than a silent hang, and paying is
diagnosing, not fixing. The receipt **opens with a top-line
banner naming the unavailable server** (`unavailable: <server>`) so degraded
never reads as clean — the absence of evidence is not evidence of absence.
When `[servers] auto_install` is already handling that server, its banner line
says so in the present tense — `installing in background (auto_install); this
round served without it, re-run to include it`, or, if the install failed,
`install failed; catenary install <server> retries by hand, the next session
start retries automatically` — so the re-run *is* the status check.
An all-unverified run can never render as empty stdout (mistakable for a
hang), and the exit stays `0`: the run completed and its receipt is
truthful. The gate releases the degraded file exactly as a paid one — editing
it again re-arms it, and a server that is back next run resumes the normal
contract. When nothing was edited it prints `[no edited files]`.

**The ledger survives a killed client.** A `catenary diagnostics` run pays its
debt by *delivery*, not at dispatch: a file's ledger entry is unlinked only after
the daemon's response reaches the CLI. So an invocation killed after dispatch (a
backgrounded command reaped by the host, a tool-call timeout, a Ctrl-C) leaves
the ledger intact and the gate armed — the next bare run re-diagnoses it in full.
Recovery is always "run it again."

**The ledger survives the daemon.** It is durable on-disk state under the
workspace root's lock directory, not in-memory: it outlives daemon restarts,
machine reboots, and its author. A fresh daemon serves the same durable ledger,
so unpaid debt is never silently forgotten across a restart. Enforcement,
however, is **payability-gated** (the nag-never-hostage rule): the edit-debt
Bash gate blocks non-edit commands only while the daemon is reachable and can
actually serve a `catenary diagnostics`. When the daemon is down the gate stands
down and work proceeds — a healthy daemon resuming the nag against the durable
ledger is the contract working, never a hostage-taking. This supersedes the old
drop-debt-on-daemon-death behavior: its intent (never lock a session out) is
preserved by payability, while the debt itself now persists.

**Reading the receipt.** `catenary diagnostics` is a diagnose, not a verdict:
its exit code is `0` whether the receipt is clean or dirty. Conditioning on the
receipt content is the caller's job, not Catenary's — a `catenary diagnostics`
followed by a commit proceeds regardless of what the receipt reports.

**Ownership.** The ledger is a property of the root, and each root has at most
one editor (the durable lock, one-cook-per-kitchen). Only the lock **holder** may
pull a locked root's ledger with the **bare** form: a non-owner's bare
`catenary diagnostics` is denied, naming the owed root and teaching `catenary
claim <root>` to take it (and its debt) over. The gate vets **every** kitchen a
bare run would pull, and the serve pulls only kitchens attributable to one
holder — another agent's root never rides along. When your cwd is outside any
root (or your cwd's root is unlocked), the bare run still serves your booked
debt so long as its attribution is unambiguous (all debt-holding kitchens share
one holder) — the ledger, not the cwd, is the truth; with multiple holders'
debt and no anchor, it serves nothing extra and answers `[no edited files]`
(stand in one of your kitchens, or scope). The **scoped** form serves named
paths regardless of ownership — a diagnose of a named file is a read, not a
payment against someone else's kitchen. On a **hookless** box (no plugin, a
scripted or CI invocation) the bare owner gate does not apply; the scoped form is
the CLI-only lint surface — `doctor` → `pin` → `diagnostics .` works with no host
plugin at all:

- **Bare** `catenary diagnostics` pays the edited set for every kitchen holding
  the caller's debt (the cwd's root leading); on empty ledgers it answers
  `[no edited files]`.
- **Scoped** `catenary diagnostics <path…>` — including `catenary diagnostics .`
  — **serves the diagnostics on demand**: it diagnoses the named paths (mounting
  an enclosing project root ephemerally when needed) and prints the receipt.

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
silence — and a named path that exists is **always served**: scoped
diagnostics is a diagnostics service, and the explicitly named path is the
intent signal. When the path has a detectable enclosing project root (walking
`.git` up from it), Catenary **mounts that root ephemerally** and diagnoses
the file from the freshly-attached server; when no marker is detectable, the
named directory itself (a file's containing directory) mounts instead —
exactly the root `catenary pin` on it would mount. Either way the mount
expires after a few minutes of inactivity (or `catenary pin` pins it), and a
file no language server covers still answers `[no LSP coverage]` honestly.
The receipt names a path on its own line only when it could not be served at
all: it does not exist (`path does not exist`), or the mount was refused
because the path is on the sensitive-path denylist.

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

Table rows carry a date-bearing local timestamp (`YYYY-MM-DDTHH:MM:SS`), so
rows stay unambiguous across a multi-day window like `--since 7d`.

`--format json` emits the **raw firehose records** — the field names are the
on-disk keys, not the table headers. In particular there is no `time` key (the
timestamp is `ts`, RFC3339 millis in UTC) and no `session` key (the session id
rides in `scope_id`, which is self-describing: a session id, a search UUID, a
`server@root`, or an instance id, depending on the record's scope). A record
with no value for an optional key omits the key entirely rather than emitting
an empty string — so a jq extraction like `.time // empty` coming back blank
means the key name is wrong, not that the field went unpopulated.

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

#### Pins persist across a restart

A pin is durable operator intent, so it survives a daemon restart. `catenary pin`
records the root in your user config's `[roots] pinned` array (and `catenary
unpin` removes it), a comment-preserving edit that leaves the rest of your
hand-authored config untouched. At the next daemon start each entry is re-added as
a pin — a zero-cost restore: a root becomes a tracker entry and a roots-board line
but spawns no language server until its first use.

**Hand-edits are first-class.** Adding a path to the `[roots] pinned` array is
itself a pin; it takes effect at the next daemon start. Home-prefixed paths render
with `~` to match the file's idiom, and entries are compared canonically (one
spelling per root):

```toml
[roots]
pinned = [
  "~/Projects/Catenary",
  "~/Projects/Lattice",
]
```

A pinned path that is **missing at boot** (a deleted repo, an unmounted volume) is
kept in the config — Catenary never rewrites your config outside an explicit
`pin`/`unpin`, so a transiently absent mount stays pinned — and `catenary doctor`
flags it. Remove it deliberately with `catenary unpin <path>` (or by deleting the
line) when the root is gone for good.

MCP workspace roots are not persisted this way: a live client re-asserts them on
each connect, so they never come from a snapshot.

### `catenary worktree`

Manage Catenary-created worktrees — the sanctioned replacement for `git
worktree` (which the agent surface denies). A worktree is a durable, isolated
checkout of a branch that language servers index like any other root, so you can
prepare a change in isolation and land it when it is ready.

```bash
catenary worktree add my-feature          # create a feats-class worktree for a branch
catenary worktree ls                      # list Catenary-managed worktrees
catenary worktree rm <path>               # remove a worktree
catenary worktree rm --force <path>       # discard a superseded dirty worktree
```

| Subcommand | Description |
|------------|-------------|
| `add <branch> [path]` | Create a durable worktree for `<branch>` (default path under Catenary's state dir; pass an explicit `path` to override). Adds a sibling symlink for discovery. |
| `ls` | List Catenary-managed worktrees — path, class, creator, age, clean/dirty, and (for `feats` worktrees) ahead/behind counts. |
| `rm <path>` | Remove a worktree class-appropriately. A dirty worktree is never auto-reaped — `rm` refuses to discard uncaptured work. `--force` is the explicit exception: it discards a dirty (superseded or abandoned) worktree through the proper disposal path — retiring the root and sweeping the registry — and names the dropped work. |

**Landing is git-native.** Worktree work lands with git itself, not a Catenary
verb:

1. commit the work in the worktree, on its branch
2. review it: `git diff main...<branch>`
3. in the owning repo: `git merge --squash <branch>`
4. commit the result
5. remove the worktree: `catenary worktree rm <path>`

The merge bracket transfers any unpaid worker debt automatically; pay it with
`catenary diagnostics`. A dirty worktree is never removed automatically:
unlanded work is always kept until you land or explicitly remove it.

The former `catenary worktree diff` / `catenary worktree land` verbs are
retired: the land patch engine (`git apply` reimplementing `git merge`) refused
on routine owning-repo drift, so both verbs are transition-period teaching
stubs that print the git-native flow above and exit `2`. The stubs will be
deleted in a later release.

> **Agent surface:** on hosts whose installed hook set registers the
> `WorktreeCreate` hook (today Claude Code), agent-side `catenary worktree add`
> is denied with a teaching message — the sanctioned way for an agent to get an
> isolated worktree is dispatching a subagent with the Agent/Task tool's
> `isolation: "worktree"`, which creates, relocates, and anchors the worktree
> itself. A hand-run add anchors nothing, leaving the subagent pinned to the
> main tree. On the same hosts, agent-side `catenary worktree rm --force` — the
> dirty-discard lever — is denied too (misc 188): discarding uncommitted work
> is the maintainer's lever, not the agent's. The git-native landing flow and
> bare `rm` (which refuses a dirty worktree on its own) stay available, and
> human terminal use is unaffected — hooks only filter agent tool calls, so
> operators keep `--force`.

### `catenary doctor`

Verify language servers and hook installation. See [Installation](installation.md#verify).

The report opens with a **Service** line: whether the always-on service is
installed (see [`catenary service`](#catenary-service)) and, when a daemon is
running, its idle footprint — the resident-set figure `MALLOC_ARENA_MAX=2`
bounds. With the daemon down there is no footprint to read, reported honestly.

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

### `catenary start` / `stop` / `restart` / `quit`

The daemon-lifecycle verbs. Their contract rides a tiny intent marker
(`daemon.intent` under the runtime dir) that every bridge consults when the
daemon is unreachable — so a bridge always knows whether a dead daemon is a
crash to recover from, a deliberate stop to wait out, or a quit to obey:

| Marker | Written by | Bridges |
|--------|------------|---------|
| absent | a crash (or `restart`) | respawn the daemon and reconnect on their own |
| `stop` | `catenary stop` | wait, never spawning, until `catenary start` |
| `quit` | `catenary quit` | end their sessions (live ones at socket loss, new ones at spawn) |

`catenary start` brings the daemon up explicitly and is the one resume verb:
it clears any stop/quit marker first, then starts (or connects to) the daemon
through the same single-instance path the bridge uses. Idempotent — if a
daemon is already running it connects, reports that, and leaves it running.

```sh
catenary start   # clear any stop/quit intent, bring the daemon up (idempotent)
```

`catenary stop` stops the daemon — and keeps it stopped. It records the `stop`
marker *before* the shutdown, so bridges wait instead of respawning; resume
with `catenary start`, or bounce with `catenary restart`. In an interactive
terminal with sessions still connected, it prints the session board first —
each connected session's host, workspace root(s), and how long it has been
connected (read from the `state.json` snapshot) — and asks for confirmation.
Declining (the default) exits `0` with the daemon left running. `--force`
skips the prompt, and a non-interactive stdin skips it too.

```sh
catenary stop            # confirm before disconnecting live sessions
catenary stop --force    # skip the prompt (scripts)
```

`catenary restart` is stop → start as one command. It writes no marker (and
clears any leftover one), so the old daemon's death reads as a crash and live
bridges reconnect through it — and it starts the new daemon itself, so it
works with no sessions connected at all. No confirmation prompt: a restart is
a bounce, not an outage. `make install` uses it to bounce onto a freshly
installed binary.

```sh
catenary restart   # bounce: old daemon down, new daemon up, bridges reattach
```

`catenary quit` is the shutdown verb: it records the `quit` marker, then stops
the daemon — live bridges end their sessions at socket loss, and new ones exit
at spawn. Affected sessions show catenary as a failed MCP server until
`catenary start` plus a fresh session (or a host retry). It confirms like
`stop`; `--force` skips.

```sh
catenary quit            # confirm, then stop the daemon and end bridge sessions
catenary quit --force    # skip the prompt
```

| Flag | Description |
|------|-------------|
| `--force` | `stop`/`quit`: skip the confirmation prompt, even with live sessions |

### `catenary service`

Install the daemon under your host's per-user service manager so it runs
always-on — resident and restarted for you, instead of spawned on demand. The
daemon no longer exits itself when the last session disconnects; its lifetime
belongs to the service manager (or to a deliberate `catenary stop`).

| Platform | Manager | Unit |
|----------|---------|------|
| Linux | systemd `--user` | `$XDG_CONFIG_HOME/systemd/user/catenary.service` |
| macOS | launchd (per-user LaunchAgent) | `~/Library/LaunchAgents/dev.markwells.catenary.plist` |

Both carry `Environment=MALLOC_ARENA_MAX=2`: the arena cap bounds the resident
set the always-on daemon accretes (a small-allocation free retained ~82 of 94
MiB in glibc arenas — the measured protocol-churn class), and the unit/plist
body carries that citation.

```sh
catenary service install     # write the unit/plist, enable it, and start it
catenary service status      # installed? active? — honest in both states
catenary service uninstall   # stop, disable, and remove it
```

The service is an **upgrade, not a requirement**. With none installed, the
`SessionStart` hook still ensures a daemon on demand — today's behavior — so
Catenary works whether or not you install the service. `catenary doctor` reports
the service state and, when a daemon is up, its idle footprint (the RSS figure
the arena cap bounds).

Writing the unit and starting it are separate steps: the unit/plist file is the
durable artifact and always lands, while the live enable/start leg rides your
user service manager and is reported (not fatal) — a headless box or a sandbox
with no user bus still gets the file, with a note to start it once a manager is
available.

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
