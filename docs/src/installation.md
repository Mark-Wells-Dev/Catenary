# Installation

## Prerequisites

- [Rust toolchain](https://rustup.rs/) (for building from source)
- Language servers for the languages you want to use (see [Language Servers](lsp/README.md))

## Install Catenary

```bash
cargo install --git https://github.com/TwoWells/Catenary catenary-mcp
```

## Connect to Your AI CLI

> The `catenary` binary must be on your PATH before configuring any client.
> Plugins and extensions provide hooks and MCP server declarations but do
> not include the binary.

### Claude Code (recommended: plugin)

```bash
claude plugin marketplace add TwoWells/Catenary
claude plugin install catenary@catenary
```

The plugin registers hooks for editing enforcement, command filtering,
and agent lifecycle tracking, plus an MCP connection for session
management and workspace root discovery.

### Gemini CLI (recommended: extension)

```bash
gemini extensions install https://github.com/TwoWells/Catenary
```

The extension registers hooks and the MCP connection.

### Manual MCP registration

For other clients, or if you prefer manual setup:

```json
{
  "mcpServers": {
    "catenary": {
      "command": "catenary"
    }
  }
}
```

This registers the MCP connection only. Without the plugin/extension,
you will not get editing state enforcement or command filtering.

## Verify

```bash
catenary doctor
```

For each configured server, `doctor` reports:

| Status | Meaning |
|--------|---------|
| `ready` | Server spawned, initialized, and capabilities listed |
| `command not found` | Binary not on `$PATH` |
| `spawn failed` | Binary found but process failed to start |
| `initialize failed` | Process started but LSP handshake failed |

Use `--root` to check a different workspace:

```bash
catenary doctor --root /path/to/project
```

For detailed diagnostics on a single server (resolved command, stderr
capture, full init request/response, capabilities):

```bash
catenary doctor rust-analyzer
```

## Next Steps

1. [Configure](configuration.md) your language servers
2. [Install language servers](lsp/README.md) for your languages
