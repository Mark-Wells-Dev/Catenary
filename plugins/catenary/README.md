# Catenary

LSP-powered code intelligence for AI coding agents. Catenary manages a
pool of language servers and exposes them through CLI commands (`grep`,
`glob`, `editing start`, `editing stop`) and hooks (editing enforcement,
command filtering). Multiple agents share the same LSP servers via a
single daemon.

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

The plugin registers hooks for editing enforcement, command filtering,
and agent lifecycle tracking, plus an MCP connection for session
management and workspace root discovery.

## Configuration

See `config.example.toml` in this directory or the [Official Configuration Guide](https://twowells.github.io/catenary/configuration.html).

## Documentation

For full documentation, please visit the **[Main Repository](https://github.com/TwoWells/Catenary)**.
