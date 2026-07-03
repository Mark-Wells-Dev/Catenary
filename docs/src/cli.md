# CLI & Dashboard

## Dashboard (TUI)

Running `catenary` in an interactive terminal launches the TUI dashboard.
When stdin and stdout are pipes (launched by an MCP client), it serves
MCP instead — no flags needed.

The dashboard is the primary way to observe Catenary at a glance. It
reads a daemon-owned `state.json` snapshot and renders four live boards:
the LSP servers and their health, the connected sessions, recent
activity, and recent alerts. It is a pure file reader — it never connects
to the daemon or opens the firehose. Full protocol and trace history
streams to an append-only JSONL telemetry firehose, which `catenary
query` reads after the fact.

```bash
catenary  # launch dashboard
```

### Keybindings

The dashboard renders four boards — **Servers** and **Sessions** on the
left (with a collapsible **Keybinds** panel) and **Activity** and
**Alerts** on the right. Navigation is keyboard-first:

| Key | Action |
|-----|--------|
| `j` / `Down` | Move down one entry |
| `k` / `Up` | Move up one entry |
| `Tab` | Focus the next board |
| `Shift+Tab` | Focus the previous board |
| `g` / `Home` | Jump to the first entry |
| `G` / `End` | Jump to the last entry |
| `PageDown` | Page down |
| `PageUp` | Page up |
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
catenary grep "TODO" --exclude-pattern "vendor/**"
catenary grep "pattern" --include-hidden --include-gitignored
catenary grep "TODO" --count            # "N matches in M files"
```

Quote glob patterns so Catenary expands them gitignore-aware rather than
the shell. Output is complete every time — no truncation, paging, or spill
files — and composes freely with pipes and redirects (`catenary grep p |
head` works). Ask for a total with `--count` and narrow with
`--exclude-pattern`.

| Flag | Description |
|------|-------------|
| `[PATH]...` | File or directory path(s) to scope the search (quoted globs allowed) |
| `--exclude-pattern <pat>` | Glob pattern to exclude from matches |
| `--count` | Report the match count instead of results |
| `--include-gitignored` | Include files ignored by .gitignore |
| `--include-hidden` | Include hidden files and directories |

### `catenary glob`

Browse the workspace: file outline, directory listing, or glob pattern
match. Auto-detects intent from the pattern — a file path shows a symbol
outline, a directory path shows a listing with symbols, and a glob
pattern shows matching files. Uses the shell's current working directory
as the base for relative patterns.

```bash
catenary glob "src/"
catenary glob "src/main.rs"
catenary glob "**/*.toml"
catenary glob "**/*.rs" --exclude-pattern "tests/**"
catenary glob "**/*.rs" --count          # "N paths"
```

Like `catenary grep`, glob emits complete output — pipe or redirect it
freely — and `--count` answers "how many" without the listing.

| Flag | Description |
|------|-------------|
| `--exclude-pattern <pat>` | Glob pattern to exclude from results |
| `--count` | Report the path count instead of results |
| `--include-gitignored` | Include files ignored by .gitignore |
| `--include-hidden` | Include hidden files and directories |

### `catenary diagnostics`

Print LSP diagnostics for the files you've edited, or lint the paths you
name. Editing is tracked automatically — the first edit to a
server-covered file starts it, there is no start step. Bare, this command
is the *end* of an edit batch: it opens every modified file on its server,
waits for each to settle, and prints a **per-file receipt** — every
diagnosed file listed, its errors and warnings beneath it, or `[clean]`
beside it when the file is clean — then clears the set. When nothing was
edited it prints `[no edited files]`.

```bash
catenary diagnostics                 # the whole edited set
catenary diagnostics src/main.rs     # lint one file on demand
catenary diagnostics src/ lib.rs     # a scoped set (relative to cwd)
```

**The edit gate is a debt paid by *diagnosing*, not fixing.** Every
server-covered file you edit joins the gate; each file's debt is cleared
by *looking at* it — pulling its diagnostics, clean or dirty — after which
you choose whether to fix. Bare pays the whole set at once. Naming paths
pays exactly those and drops them from the gate: a **partial** pull leaves
the gate **armed** for the files you didn't name, so the command filter
keeps blocking unrelated commands until the rest are diagnosed. Editing a
paid file re-arms it. A named path that was never edited is simply linted
on demand — it pays nothing, since it owed nothing. Relative paths resolve
against the shell's current working directory.

`catenary diagnostics` is a load-bearing command — run it (bare or scoped)
as its **own step** (no pipes, no `&&`/`;` chaining), and read the result.
**The exit code is a trust signal, not a lint result:** it exits `0`
whenever the run completed — clean *or* dirty — and `2` only on a genuine
fault (no daemon, IPC failure). It never exits `1`, so a run that found
errors is not mistaken for a failed call — read the receipt for the
errors, not the exit code. (Whether a run is labeled "dirty" is tunable
via `diagnostics_severity` in [Configuration](configuration.md#diagnostics),
but that is a status label only and does not change the exit code.)

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

### `catenary doctor`

Verify language servers and hook installation. See [Installation](installation.md#verify).

Pass a server name for verbose single-server diagnostics:

```sh
catenary doctor rust-analyzer
```

Verbose mode prints the resolved command, binary path, stderr capture,
full `initialize` request/response JSON, and capabilities list.
