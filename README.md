# Catenary

[![CI](https://github.com/TwoWells/Catenary/actions/workflows/ci.yml/badge.svg)](https://github.com/TwoWells/Catenary/actions/workflows/ci.yml)
[![CD](https://github.com/TwoWells/Catenary/actions/workflows/cd.yml/badge.svg)](https://github.com/TwoWells/Catenary/actions/workflows/cd.yml)

**Enforced code intelligence.**

Catenary hands an AI coding agent a small, opinionated set of
code-intelligent commands — and a hook that keeps it on them. Reach for
`grep` and you're redirected to `catenary grep`; reach for `sed -i` and
you get `catenary sed`. Every command the agent can run is backed by a
language server, so it navigates code by meaning instead of brute-forcing
text. The generic path isn't blocked for safety — it's off the menu, so
the code-intelligent one is the only path left.

## Why Catenary

Exposing language-server tools to an agent isn't novel anymore — most
coding CLIs do it, and Catenary did it early. But *having* a tool and
*using* it are different things. Give an agent `grep`, `find`, raw file
reads, **and** LSP navigation, and it reaches for whatever's nearest —
usually brute-force text scanning that burns context and misses structure.

Catenary takes the choice away. It exposes one curated, code-intelligent
surface and enforces it:

- A `PreToolUse` hook runs an **allowlist** over every shell command the
  agent issues.
- Denied commands aren't dead ends — each denial **names the
  code-intelligent command to run instead** (`grep` → `catenary grep`,
  `ls`/`find` → `catenary glob`, `sed -i` → `catenary sed`).
- Edits flow through the host's tracked Edit/Write tools, so LSP
  diagnostics come back automatically when the agent runs `catenary
  diagnostics`.

The result is a workflow the agent follows by construction, not by
prompt-engineering.

**This is enforcement of a workflow, not a security sandbox.** The hook is
a cooperative contract — a Makefile can still run anything, and Catenary
doesn't isolate the filesystem or environment. Its job is narrower and
more useful: keep the agent's reads on code-intelligent search and its
writes on a tracked path, so navigation is structural and diagnostics are
free.

### Less context, more signal

A grep-and-read loop pulls whole files into the agent's context window and
re-processes them every turn. `catenary grep` answers with the symbol, its
signature, and where it's used — tens of tokens instead of thousands.
Diagnostics arrive through `catenary diagnostics` stdout, so the agent
never re-reads a file just to check whether its edit compiled.

## The surface

**Search** — always available, no setup beyond installation:

| Command | What it does |
|---------|--------------|
| `catenary grep <pattern>` | Symbol, reference, and text search — LSP-enriched within tracked roots |
| `catenary glob <path>` | File outlines, directory listings, glob matches |
| `catenary sed <pat> <repl> <path>` | Tracked regex find-and-replace — the mass-edit surface |

**The edit → diagnostics loop** — run in the host's shell tool:

```bash
# Edit files with the host's native Edit/Write tools. Editing starts
# automatically on the first change to a server-covered file — there is
# no start step.
catenary diagnostics      # print LSP diagnostics for every file you
                          # touched, then clear the set
```

`catenary diagnostics` is the *end* of an edit batch: it opens the
modified files on their servers, waits for each to settle, and prints the
errors and warnings — like a linter, it's silent on success. For sweeps
too broad for per-file edits, `catenary sed --in-place` folds the changed
files into the same diagnostics batch.

**Workspace roots** — manage which directories are indexed:

```bash
catenary roots add <path>   # index a directory
catenary roots rm <path>
catenary roots ls
```

## How it fits together

One daemon per host manages a shared pool of language servers. Multiple
agents connect over a Unix socket and share those servers — a single
`rust-analyzer` serves every session on the same project. Catenary reaches
the agent through four decoupled surfaces; none depends on the others.

```
agents ──▶  Catenary daemon  ──▶  shared LSP server pool
                                  rust-analyzer · pyright · gopls · …

reached through four decoupled surfaces:

  CLI     grep · glob · sed · diagnostics   — via the host's shell tool
  Hooks   allowlist enforcement             — one PreToolUse hook
  MCP     heartbeat + workspace roots       — no query tools
  TUI     live observability                — protocol & trace traffic
```

- **CLI** — the code-intelligent commands above, invoked through the
  host's shell tool. Stateless: each command connects to the daemon,
  delegates to the right language servers, and prints to stdout.
- **Hooks** — the `PreToolUse` allowlist that enforces the workflow and
  tracks edited files for the diagnostics batch.
- **MCP** — a heartbeat only: the protocol handshake, the workspace-roots
  channel, and user-facing notifications. It advertises **no** query tools.
- **TUI** — real-time observability across every session and language
  server.

## Quick start

### 1. Install

```bash
cargo install catenary-mcp
```

The `catenary` binary must be on your `PATH` before configuring any
client. Plugins and extensions provide hooks and the MCP declaration but
**do not include the binary** — this step is required.

### 2. Configure language servers

Add your language servers to `~/.config/catenary/config.toml`:

```toml
[language.rust]
command = "rust-analyzer"

[language.python]
command = "pyright-langserver"
args = ["--stdio"]
```

### 3. Connect your agent

**Claude Code**
```bash
claude plugin marketplace add TwoWells/Catenary
claude plugin install catenary@catenary
```

**Gemini CLI**
```bash
gemini extensions install https://github.com/TwoWells/Catenary
```

**Antigravity CLI** — copy `plugins/catenary-antigravity/` to
`.agents/plugins/catenary/` in your workspace.

### 4. Verify

```bash
catenary doctor
```

`doctor` reports each configured server's status (`ready`, `command not
found`, `spawn failed`, `initialize failed`) and whether the host's hooks
are installed and current. Pass a server name (`catenary doctor
rust-analyzer`) for verbose single-server diagnostics.

## Observability

Run `catenary` in a terminal to launch the TUI dashboard — a live view of
every session, every language server, and the protocol traffic between
them. Catenary keeps a `state.json` snapshot of live state and streams
full protocol and trace detail to a sharded JSONL telemetry firehose; the
TUI reads the snapshot, and `catenary query` reads the firehose. (There is
no SQLite database — a legacy one is drained on startup.)

| Command | Description |
|---------|-------------|
| `catenary` | Launch the TUI dashboard |
| `catenary query` | Query the telemetry firehose (by session, server, tool, time, …) |
| `catenary doctor` | Verify language servers and hook installation |
| `catenary version` | Show the CLI and running-daemon versions |
| `catenary stop` | Stop the running daemon |

## Documentation

Full documentation at **[twowells.github.io/Catenary](https://twowells.github.io/Catenary/)**

- **[Installation](https://twowells.github.io/Catenary/stable/installation.html)** — setup for Claude Code, Gemini CLI, and Antigravity CLI
- **[Configuration](https://twowells.github.io/Catenary/stable/configuration.html)** — language servers, routing, command allowlist
- **[CLI & Dashboard](https://twowells.github.io/Catenary/stable/cli.html)** — the command surface and TUI dashboard

## License

**AGPL-3.0-or-later** — see [LICENSE](LICENSE) for details.

**Commercial licensing** available for proprietary use — see
[LICENSE-COMMERCIAL](LICENSE-COMMERCIAL). Contact `contact@markwells.dev`.
