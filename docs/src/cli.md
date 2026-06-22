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
catenary grep "TODO" --exclude-pattern "vendor/**" --page 2
catenary grep "pattern" --include-hidden --include-gitignored
catenary grep "TODO" --count            # "N matches in M files"
```

Quote glob patterns so Catenary expands them gitignore-aware rather than
the shell. `catenary grep` owns its output: page through large results
with `--page` and ask for a total with `--count` — don't pipe the output
through `head`/`tail`/`wc`.

| Flag | Description |
|------|-------------|
| `[PATH]...` | File or directory path(s) to scope the search (quoted globs allowed) |
| `--exclude-pattern <pat>` | Glob pattern to exclude from matches |
| `--page <n>` | Page number for paged results (default: 1) |
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
catenary glob "**/*.rs" --exclude-pattern "tests/**" --page 2
catenary glob "**/*.rs" --count          # "N paths"
```

Like `catenary grep`, glob owns its output — use `--page` and `--count`
instead of piping into `head`/`tail`/`wc`.

| Flag | Description |
|------|-------------|
| `--exclude-pattern <pat>` | Glob pattern to exclude from results |
| `--page <n>` | Page number for paged results (default: 1) |
| `--count` | Report the path count instead of results |
| `--include-gitignored` | Include files ignored by .gitignore |
| `--include-hidden` | Include hidden files and directories |

### `catenary sed`

Regex find-and-replace across files — the tracked mass-edit surface for
sweeps too broad for the host's Edit tool. Previews by default (resolved
file list plus per-file match counts, writing nothing); `--in-place`
applies the edits and folds the changed files into the diagnostics batch,
so run `catenary diagnostics` afterward. Capture groups are `$1` (not
`\1`); `\n`/`\t`/`\r` are interpreted.

Single-quote the pattern and replacement so the shell leaves regex
metacharacters and `$1` capture references intact:

```bash
catenary sed 'old_name' 'new_name' 'src/**/*.rs'            # preview
catenary sed 'old_name' 'new_name' 'src/**/*.rs' --in-place
catenary sed '(\w+)_old' '$1_new' 'src/' --in-place
catenary sed 'Foo' 'Bar' 'src/' --in-place --preserve-case  # Foo→Bar, foo→bar
```

| Flag | Description |
|------|-------------|
| `[PATH]...` | File/directory path(s) or quoted glob pattern(s). Required — never rewrites the whole tree implicitly |
| `--in-place` | Apply the edits (default: preview) |
| `--ignore-case` | Case-insensitive matching |
| `--preserve-case` | Case the replacement to match each hit |
| `--first` | Replace only the first match per file (default: all) |
| `--exclude-pattern <pat>` | Glob pattern to exclude from matches |
| `--page <n>` | Page number for the paged preview |
| `--include-gitignored` | Include files ignored by .gitignore |
| `--include-hidden` | Include hidden files and directories |

### `catenary diagnostics`

Print LSP diagnostics for every file you've edited since the last run,
then clear the set. Editing is tracked automatically — the first edit to
a server-covered file starts it, there is no start step — so this command
is the *end* of an edit batch: it opens all modified files on their
servers, waits for each to settle, and prints the errors and warnings —
listing only the files that have them. Like a linter (`ruff`, `clippy`,
`eslint`), it is **silent on success**: a clean batch prints nothing and
exits 0.

```bash
catenary diagnostics
```

`catenary diagnostics` and `catenary sed --in-place` are load-bearing,
correlated commands — run each **bare**, as its own step (no pipes, no
`&&`/`;` chaining), and read the result. While a batch of covered edits is
pending, the command filter blocks unrelated commands until you run
`catenary diagnostics`. The exit code signals compile state: non-zero when
the run is "dirty" (an error by default — see `diagnostics_severity` in
[Configuration](configuration.md#diagnostics)).

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
