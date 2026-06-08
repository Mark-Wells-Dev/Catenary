# CLI & Dashboard

## Dashboard (TUI)

Running `catenary` in an interactive terminal launches the TUI dashboard.
When stdin and stdout are pipes (launched by an MCP client), it serves
MCP instead — no flags needed.

The dashboard is the primary way to observe Catenary. It shows all
sessions (active and historical), their language servers, and a live
stream of protocol messages (MCP, LSP, hooks). All messages are stored
in a SQLite database, so historical sessions can be browsed after the
fact.

```bash
catenary  # launch dashboard
```

### Keybindings

Keybinding hints appear in each pane's border.

**Sessions pane:**

| Key | Action |
|-----|--------|
| `j` / `Down` | Next session |
| `k` / `Up` | Previous session |
| `Space` | Toggle expand/collapse |
| `h` / `l` | Scroll horizontally (events) |
| `r` | Refresh |
| `x` | Delete session data (dead sessions only) |
| `q` / `Esc` | Quit |

**Events pane:**

| Key | Action |
|-----|--------|
| `j` / `Down` | Next event |
| `k` / `Up` | Previous event |
| `Space` | Toggle expand/collapse |
| `h` / `l` | Scroll horizontally |
| `Ctrl-u` | Page up |
| `Ctrl-d` | Page down |
| `G` | Jump to latest |
| `y` | Yank selected event |
| `f` | Open filter input |
| `F` | Clear filter |

## Protocol Transparency

Catenary logs every protocol message — every MCP exchange, every LSP
request and response, every hook invocation — to a local SQLite database.
The TUI shows the full message flow in real time: what Catenary sends to
your language servers, what they send back, and how long each exchange
takes.

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
servers, waits for each to settle, and prints the errors and warnings (or
`[clean]` when there are none).

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

### `catenary debug`

Diagnostic and debugging tools for inspecting session data, grouped under
the `debug` subcommand: `list`, `monitor`, `status`, `query`, and `gc`.

#### `catenary debug list`

List active and historical sessions.

```bash
catenary debug list
```

#### `catenary debug monitor <id>`

Stream events from a session to the terminal. Accepts a prefix of
either the Catenary session ID or the host CLI session ID.

```bash
catenary debug monitor 029b
catenary debug monitor 029b --raw       # raw JSON output
catenary debug monitor 029b --filter hover
```

#### `catenary debug status <id>`

Show the status of a single session.

```bash
catenary debug status 029b
```

#### `catenary debug query`

Query events from the database. Useful for debugging and bug reports.

```bash
catenary debug query --session 029ba740 --since 1h
catenary debug query --kind diagnostics --since today
catenary debug query --search "hover" --format json
catenary debug query --sql "SELECT * FROM events WHERE payload LIKE '%timeout%'"
```

#### `catenary debug gc`

Garbage-collect old session data.

```bash
catenary debug gc --older-than 7d
catenary debug gc --dead
catenary debug gc --session 029ba740
```

### `catenary doctor`

Verify language servers and hook installation. See [Installation](installation.md#verify).

Pass a server name for verbose single-server diagnostics:

```sh
catenary doctor rust-analyzer
```

Verbose mode prints the resolved command, binary path, stderr capture,
full `initialize` request/response JSON, and capabilities list.
