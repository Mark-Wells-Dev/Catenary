# Installation

## Prerequisites

- [Rust toolchain](https://rustup.rs/)
- Language servers for the languages you want to use (see [Language Servers](lsp/README.md))

## Install Catenary

```bash
cargo install catenary-mcp
```

To build from source instead:

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

## Disabling Catenary per project

Catenary runs one daemon per host, shared across every project you open. If you
want its enforcement off in a single project — while it keeps serving every
other project — most hosts let you switch off the plugin, extension, or hook set
for that project alone. This is an option, not a recommendation.

What disabling turns off, in that project only: the hooks (editing enforcement,
command filtering, file tracking) and, where the host's plugin also carries it,
the MCP session wiring. What stays: there is nothing to uninstall, the daemon
keeps running and serving your other projects, and the `catenary` binary and its
CLI commands (`grep`, `glob`, `diagnostics`) still work if you invoke them by
hand. Re-enabling resumes cleanly — editing state is per-session, so nothing
stale persists; the next enabled run starts tracking from a clean slate.

### Claude Code

Set the plugin to `false` in the project's `.claude/settings.json`:

```json
{
  "enabledPlugins": {
    "catenary@catenary": false
  }
}
```

Committed to the repository, this disables the Catenary plugin for everyone who
opens the project — project settings override user settings. For a personal,
uncommitted opt-out, put the same block in `.claude/settings.local.json`
(git-ignored, and higher precedence still). The precedence order is Local >
Project > User, so either file overrides a plugin enabled in your
`~/.claude/settings.json`. `claude plugin disable catenary@catenary --scope
project` writes the same entry for you. Disabling the plugin stops both its
hooks and its MCP connection for that project.

See the [Claude Code plugin
docs](https://code.claude.com/docs/en/discover-plugins) and [settings
precedence](https://code.claude.com/docs/en/settings).

### Gemini CLI

Disable the extension for the current workspace:

```bash
gemini extensions disable catenary --scope workspace
```

`--scope workspace` scopes the change to the current project; omit it (or pass
`--scope user`) to disable Catenary everywhere. The change takes effect on the
next CLI restart, and turns off the extension's hooks and its MCP server for
that workspace. See the [Gemini CLI extension
reference](https://geminicli.com/docs/extensions/reference/).

### OpenCode

OpenCode has no native per-plugin disable switch — there is no `disabled_plugins`
key in `opencode.json` (it is an open feature request). Plugins are auto-loaded
from a plugin directory instead, so the per-project story depends on how
Catenary was installed:

- **Installed per-workspace** (`catenary install opencode --workspace`): the
  plugin file lives in the project at `.opencode/plugin/catenary.js`. Delete
  that file to disable Catenary in this project only; other projects are
  untouched.
- **Installed globally** (`~/.config/opencode/plugin/catenary.js`): there is no
  per-project switch. Removing the global file disables Catenary in every
  OpenCode project, not just one.

You can drop the daemon heartbeat for a single project by disabling the MCP
server in that project's `opencode.json`:

```json
{
  "mcp": {
    "catenary": {
      "enabled": false
    }
  }
}
```

This only stops the session-registration heartbeat — the enforcement plugin
still runs, so it is not a full disable. See the [OpenCode plugin
docs](https://opencode.ai/docs/plugins/).

### Antigravity

Antigravity has no per-workspace disable toggle either. It discovers plugins by
location: global plugins live under `~/.gemini/config/plugins/` (where `catenary
install antigravity` places `catenary/`), and workspace plugins live under
`.agents/plugins/` (or `_agents/plugins/`) at the project root. A
globally-installed Catenary plugin therefore has no per-project off switch;
removing `~/.gemini/config/plugins/catenary` disables it in every project. See
the [Antigravity plugin docs](https://antigravity.google/docs/plugins).

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
