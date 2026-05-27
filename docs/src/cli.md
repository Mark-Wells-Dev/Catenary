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
catenary grep "foo|bar" --glob "src/**/*.rs"
catenary grep "TODO" --exclude "vendor/**" --page 2
catenary grep "pattern" --include-hidden --include-gitignored
```

| Flag | Description |
|------|-------------|
| `--glob <pat>` | Glob pattern to scope the search |
| `--exclude <pat>` | Glob pattern to exclude from matches |
| `--page <n>` | Page number for paged results (default: 1) |
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
catenary glob "**/*.rs" --exclude "tests/**" --page 2
```

| Flag | Description |
|------|-------------|
| `--exclude <pat>` | Glob pattern to exclude from results |
| `--page <n>` | Page number for paged results (default: 1) |
| `--include-gitignored` | Include files ignored by .gitignore |
| `--include-hidden` | Include hidden files and directories |

### `catenary list`

List active and historical sessions.

```bash
catenary list
```

### `catenary monitor <id>`

Stream events from a session to the terminal. Accepts a prefix of
either the Catenary session ID or the host CLI session ID.

```bash
catenary monitor 029b
catenary monitor 029b --raw       # raw JSON output
catenary monitor 029b --filter hover
```

### `catenary query`

Query events from the database. Useful for debugging and bug reports.

```bash
catenary query --session 029ba740 --since 1h
catenary query --kind diagnostics --since today
catenary query --search "hover" --format json
catenary query --sql "SELECT * FROM events WHERE payload LIKE '%timeout%'"
```

### `catenary gc`

Garbage-collect old session data.

```bash
catenary gc --older-than 7d
catenary gc --dead
catenary gc --session 029ba740
```

### `catenary doctor`

Verify language servers and hook installation. See [Installation](installation.md#verify).

Pass a server name for verbose single-server diagnostics:

```sh
catenary doctor rust-analyzer
```

Verbose mode prints the resolved command, binary path, stderr capture,
full `initialize` request/response JSON, and capabilities list.
