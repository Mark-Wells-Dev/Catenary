# Installation

## Prerequisites

- [Rust toolchain](https://rustup.rs/)
- Language servers for the languages you want to use — Catenary can install
  these itself (see [Managed language servers](#managed-language-servers))
  or you can install them system-wide (see [Language Servers](lsp/README.md))

## Platforms

Catenary ships prebuilt binaries for **Linux x86_64** and **macOS arm64**
(Apple silicon). There is **no Intel-mac binary** — the installer refuses
Intel Macs and points you at a source build — and **no Windows binary**:
the daemon's transport is Unix-socket-bound today, so Windows support
returns after the port rather than shipping unverified.

## Install Catenary

**Homebrew (macOS and Linux):**

```bash
brew install twowells/tap/catenary
```

> Switching from a `cargo install` (the previously recommended path)?
> Run `cargo uninstall catenary-cli` first — or `cargo uninstall
> catenary-mcp` if you installed before 2.1.0, when the crate carried
> that name. `~/.cargo/bin` usually precedes brew's bin dir on `PATH`,
> so the stale binary keeps answering otherwise.

**Prebuilt binary (Linux / macOS arm64):**

```bash
curl -fsSL https://raw.githubusercontent.com/TwoWells/Catenary/main/install.sh | sh
```

The script detects your platform, downloads the matching release asset
(`catenary-linux-amd64` or `catenary-macos-arm64`), and installs it to
`/usr/local/bin` (override with `CATENARY_INSTALL_DIR`). Once installed,
`catenary update` self-updates the binary in place.

**From crates.io (any platform with a Rust toolchain):**

```bash
cargo install catenary-cli
```

**From source:**

```bash
cargo install --git https://github.com/TwoWells/Catenary catenary-cli
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
management and workspace root discovery. It also owns worktree creation
(the `WorktreeCreate` hook), placing each `isolation:"worktree"` subagent
worktree outside your repo under the cache dir so language servers never
index it as a duplicate copy of your project.

### OpenCode (plugin)

```bash
catenary install opencode
```

OpenCode has no `hooks.json` surface, so Catenary ships an in-process plugin.
The install is **plugin-only**: it writes exactly one Catenary-owned file —
`~/.config/opencode/plugin/catenary.js` (the plugin) — and makes **zero edits to
your `opencode.json`**. On config load the plugin injects the MCP heartbeat
(`mcp.catenary`) and regenerates its teaching from the binary by itself, so
nothing is merged into your config. Teaching is **runtime-only** — there is no
shipped static fallback file. Pass `--workspace` to install into the project
(`.opencode/`) instead of globally.

Because the whole integration rides one plugin, there is a single disable
switch and it turns off everything together — enforcement, teaching, and the MCP
heartbeat: delete or rename that one file, `plugin/catenary.js`, or launch
OpenCode with `OPENCODE_PURE=1` (which disables **all** external plugins). See
[Disabling Catenary per project](#disabling-catenary-per-project) below.

> **Upgrading from an earlier version?** Older releases merged an `mcp.catenary`
> entry and an `instructions` reference to a rules file into your
> `opencode.json`, and shipped a static `~/.config/opencode/catenary.md`
> teaching fallback. The plugin now carries the heartbeat and regenerates its
> teaching from the binary at runtime, so those are all redundant: you may remove
> the merged `mcp.catenary` / `instructions` entries, delete
> `~/.config/opencode/catenary.md`, and drop any old `instructions` entry naming
> it. Leaving the merged `mcp.catenary` entry is harmless — the plugin defers to
> it.

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

## Managed Language Servers

> **Two installs, two meanings.** `catenary install <host>` installs *host
> integration* — it writes the host's plugin and hook files and never
> touches your Catenary config. Language servers have no install command at
> all: Catenary installs them for you — via the opt-in below, or the TUI's
> guided install — or you install them system-wide by hand
> ([Language Servers](lsp/README.md)).

Catenary can install and manage the language servers it spawns. Opt in
from your user config (`~/.config/catenary/config.toml`):

```toml
[servers]
auto_install = true
```

With the opt-in set, each session start detects configured servers your
workspace roots need that cannot spawn — nothing on PATH, no managed
install yet — and installs each one in the background, at the exact
version that passed Catenary's CI conformance gate, into a
Catenary-owned directory (`~/.local/share/catenary/servers/` on Linux).
Session start never waits: the kick is announced, and coverage arrives
when the install lands. There is no per-server install step — the config
edit is the setup. Managed installs are preferred at spawn, so a system
package upgrade can never change the binary Catenary runs; servers you
already have on PATH, and servers with an explicit `path` override, are
left alone. See [Managed Server
Installs](configuration.md#managed-server-installs) for the full contract.

The opt-in is honored from your user config only — a project
`.catenary.toml` can never switch it on. That is a consent posture: a
public repository must not be able to opt your machine into installing
software.

Prefer per-server consent? The TUI offers the same install interactively.
Run `catenary` to open the dashboard: a missing server one of your active
languages needs appears in the problems pane as an install suggestion, and
when the server is in the vetted set, pressing `a` on that row opens a
consent overlay previewing the resolved install plan — `Enter` runs it,
`Esc` dismisses. It is the same verified engine as `auto_install`, landing
the same pinned version in the same managed home. There is no CLI verb for
either path — installs happen through the opt-in or the overlay, never a
shell command. See [Guided install](cli.md#guided-install).

> **Migrating from system installs.** If you installed language servers
> system-wide solely for Catenary's use, you can retire them: remove the
> system install, and (with `auto_install` on) the next session start
> detects the server as missing and installs the vetted version into the
> managed home. A server still on PATH is never replaced, so removing it
> is what hands it over. `catenary doctor` is the residue list: a
> system-installed server whose running version differs from the vetted
> pin draws an advisory drift finding naming both versions (e.g. `taplo:
> running version 0.8.0 differs from the blessed 0.10.0`), so every
> leftover still being consulted is named.

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

### OpenCode

Catenary integrates with OpenCode through a single in-process plugin, so there
is one switch and it turns off the whole integration at once — enforcement,
teaching, and the MCP heartbeat all ride this one plugin. OpenCode has no
per-plugin disable key in `opencode.json` (it is an open feature request);
plugins are auto-loaded from a plugin directory instead, so the per-project
story depends on how Catenary was installed:

- **Installed per-workspace** (`catenary install opencode --workspace`): the
  plugin file lives in the project at `.opencode/plugin/catenary.js`. Delete or
  rename that file to disable Catenary in this project only; other projects are
  untouched.
- **Installed globally** (`~/.config/opencode/plugin/catenary.js`): there is no
  per-project switch. Removing or renaming the global file disables Catenary in
  every OpenCode project. To turn it off for a single session without deleting
  anything, launch OpenCode with `OPENCODE_PURE=1`, which disables **all**
  external plugins (not just Catenary).

There is no partial toggle — no "keep teaching, drop the heartbeat" — because
the plugin is the only integration surface. See the [OpenCode plugin
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

`doctor` resolves each server exactly as a spawn would, so a managed
install counts as installed even when nothing is on `$PATH`. It also
reports background auto-installs (running, landed, or failed), and adds
an advisory drift finding when a running server's version differs from
its vetted pin — informational only, nothing is refused (see
[Managed language servers](#managed-language-servers)).

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
2. Install language servers — opt in to
   [managed installs](#managed-language-servers), or follow the
   per-language [setup guides](lsp/README.md)
