# Catenary

[![CI](https://github.com/TwoWells/Catenary/actions/workflows/ci.yml/badge.svg)](https://github.com/TwoWells/Catenary/actions/workflows/ci.yml)
[![CD](https://github.com/TwoWells/Catenary/actions/workflows/cd.yml/badge.svg)](https://github.com/TwoWells/Catenary/actions/workflows/cd.yml)

**Multi-surface intelligence router: LSP-powered code intelligence for AI agents.**

Catenary manages a pool of LSP servers and exposes them through four
decoupled surfaces — any combination works independently:

- **MCP** — stateless search tools (`grep`, `glob`). Any MCP client
  gets LSP-backed code intelligence with no session state.
- **Hooks** — editing enforcement, command filtering, and file tracking.
  One `PreToolUse` hook per host, unified daemon IPC.
- **CLI** — editing lifecycle commands (`catenary start_editing`,
  `catenary done_editing`, `catenary add-root`, `catenary rm-root`).
  Invoked via the host's shell tool.
- **TUI** — real-time observability across all sessions and LSP servers.
  Watch what agents are actually doing.

All four surfaces share the same LSP server pool. The daemon is the
server manager — it doesn't care which surface woke it up.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Catenary Daemon                       │
│                                                         │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐    │
│  │rust-analyzer│  │   pyright   │  │    gopls    │    │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘    │
│         └────────────────┼────────────────┘            │
│                    LSP Server Pool                       │
├─────────────────────────────────────────────────────────┤
│  MCP        Hooks        CLI           TUI              │
│  (queries)  (enforce)    (editing)     (observe)        │
└─────────────────────────────────────────────────────────┘
     │            │            │              │
   Claude      Claude       Claude        Terminal
   Gemini      Gemini       Gemini
   Any MCP     Antigravity  Antigravity
   client
```

## Quick Start

### 1. Install

```bash
cargo install catenary-mcp
```

### 2. Configure language servers

Add your language servers to `~/.config/catenary/config.toml`:

```toml
[language.rust]
command = "rust-analyzer"

[language.python]
command = "pyright-langserver"
args = ["--stdio"]
```

### 3. Connect your AI assistant

> Plugins register hooks and MCP server declarations but **do not
> include the binary** — step 1 above is required.

**Claude Code**
```
/plugin marketplace add TwoWells/Catenary
/plugin install catenary@catenary
```

**Gemini CLI**
```bash
gemini extensions install https://github.com/TwoWells/Catenary
```

**Antigravity CLI**

Copy `plugins/catenary-antigravity/` to `.agents/plugins/catenary/`
in your workspace.

### 4. Verify

```bash
catenary doctor
```

```
rust         rust-analyzer       ✓ ready
             hover definition references document_symbols search code_actions call_hierarchy

python       pyright-langserver  ✓ ready
             hover definition references document_symbols search

Hooks:
  Claude Code 1.6.1 (directory) ✓ hooks match
  Gemini CLI  1.6.1 (linked)    ✓ hooks match
```

## Usage

**Search** — always available, no setup beyond installation:

| MCP Tool | Description |
|----------|-------------|
| `grep` | Symbols, semantic references, and text matches |
| `glob` | File outlines, directory listings, glob matches |

**Editing** — run in the host's shell tool:

```bash
catenary start_editing       # enter editing mode
# ... edit files with the host's native tools ...
catenary done_editing        # get LSP diagnostics for all edits
```

**Workspace roots** — manage which directories are indexed:

```bash
catenary add-root <path>     # add a workspace root
catenary rm-root <path>      # remove a workspace root
```

## Observability

Catenary logs every protocol message — MCP tool calls, LSP requests and
responses, hook invocations — to a local SQLite database. The TUI
dashboard shows the message flow in real time across all sessions.

| Command | Description |
|---------|-------------|
| `catenary` | Launch the TUI dashboard |
| `catenary monitor <id>` | Stream events from a session |
| `catenary list` | List active and historical sessions |
| `catenary query` | Query session events |
| `catenary gc` | Garbage-collect old session data |
| `catenary doctor` | Verify language servers and hook installation |

## Why This Matters

AI coding agents waste context on redundant file reads. Every file an
agent reads goes into an append-only context window. Three rounds of
read-edit-verify on one file puts three full copies in context,
re-processed on every subsequent turn.

Catenary replaces brute-force file scanning with graph navigation.
Instead of reading a 500-line file to find a type signature, the agent
asks the language server — 50 tokens instead of 2,000. Diagnostics
arrive via `catenary done_editing` stdout, so the agent never re-reads
files to check for errors.

## Documentation

Full documentation at **[twowells.github.io/Catenary](https://twowells.github.io/Catenary/)**

- **[Installation](https://twowells.github.io/Catenary/stable/installation.html)** — Setup for Claude Code, Gemini CLI, and Antigravity CLI
- **[Configuration](https://twowells.github.io/Catenary/stable/configuration.html)** — Language servers, routing, command filtering
- **[CLI & Dashboard](https://twowells.github.io/Catenary/stable/cli.html)** — TUI dashboard and CLI commands

## License

**AGPL-3.0-or-later** — See [LICENSE](LICENSE) for details.

**Commercial licensing** available for proprietary use — see [LICENSE-COMMERCIAL](LICENSE-COMMERCIAL). Contact `contact@markwells.dev`.
