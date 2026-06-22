# Catenary

**Enforced code intelligence for Claude Code.**

Catenary hands the agent a small, opinionated set of code-intelligent
commands — `catenary grep`, `glob`, `sed`, `diagnostics` — and a
`PreToolUse` hook that keeps it on them. Reach for `grep` and it's
redirected to `catenary grep`; reach for `sed -i` and you get `catenary
sed`. Every command is backed by a language server, so the agent navigates
code by meaning instead of brute-forcing text. Multiple agents share one
daemon and one pool of LSP servers.

Run `catenary primer` for the full agent-facing workflow.

## Installation

### 1. Install the binary

```bash
cargo install catenary-mcp
```

The `catenary` binary must be on your PATH. The plugin does not include it —
it only registers hooks and the MCP connection. If the binary is missing,
hooks will silently do nothing.

### 2. Install the plugin

```
/plugin marketplace add TwoWells/Catenary
/plugin install catenary@catenary
```

The plugin registers the `PreToolUse` hook (editing enforcement, command
allowlist, file tracking) and agent-lifecycle hooks, plus an MCP
connection used as a **heartbeat** — the protocol handshake and
workspace-root discovery. It advertises no query tools; the command
surface is the CLI.

## Configuration

See `config.example.toml` in this directory or the [Official Configuration Guide](https://twowells.github.io/Catenary/stable/configuration.html).

## Documentation

For full documentation, please visit the **[Main Repository](https://github.com/TwoWells/Catenary)**.
