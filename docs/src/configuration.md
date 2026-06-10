# Configuration

Catenary loads configuration from multiple sources, in order of priority
(last wins):

1. **Built-in defaults**: Server definitions (`defaults/servers.toml`) and language classification with server bindings (`defaults/languages.toml`). Common language servers work without any config — if the binary is on PATH, Catenary uses it.
2. **User config**: `~/.config/catenary/config.toml`.
3. **Project config**: `.catenary.toml` in each workspace root. Discovered when roots are added (at startup or via `catenary roots add`). Scoped to `[language.*]`, `[server.*]`, and `[commands]` `build` only — every other `[commands]` key and all other sections are user-level (see [Project-scoped commands](#project-scoped-commands)).
4. **Explicit file**: `--config <path>`.
5. **Environment variables**: Prefixed with `CATENARY_` (e.g., `CATENARY_LOG_RETENTION_DAYS=30`). Use `__` for nested keys (e.g., `CATENARY_ICONS__PRESET=nerd`).

## Language Servers

Configuration uses two sections: `[server.*]` defines how to run a
language server, and `[language.*]` binds languages to servers.

```toml
[server.<name>]
command = "server-binary"
args = ["arg1", "arg2"]

[language.<language-id>]
servers = ["<name>"]
```

### Built-in Defaults

Catenary ships built-in definitions for ~25 common language servers. If
the server binary is on PATH and the language has a default binding, LSP
intelligence works without any `[server.*]` or `[language.*]` config.

Run `catenary config` to see the full list of built-in servers.

A user-defined `[server.X]` completely replaces the built-in default for
`X` — no merging. If you define `[server.rust-analyzer]`, your definition
is used and the built-in is ignored entirely.

### Example

The built-in defaults cover the basics. You only need config for
customisation — `initialization_options`, `settings`, `env`, etc.:

```toml
# Override the built-in rust-analyzer with custom options
[server.rust-analyzer]
env = { CLIPPY_DISABLE_DOCS_LINKS = "1" }

[server.rust-analyzer.initialization_options]
check.command = "clippy"
cargo.features = "all"
diagnostics.disabled = ["inactive-code"]

# Override pyright with workspace settings
[server.pyright.settings.python]
pythonPath = "/usr/bin/python3"

[server.pyright.settings.python.analysis]
exclude = ["**/target", "**/node_modules"]
extraPaths = []
```

To define a server from scratch (or one without a built-in default):

```toml
[server.phpactor]
command = "phpactor"
args = ["language-server"]

[language.php]
servers = ["phpactor"]
```

### Initialization Options

Server-specific options passed during the LSP `initialize` request.
These go on the `[server.*]` entry:

```toml
[server.rust-analyzer.initialization_options]
check.command = "clippy"
cargo.features = "all"
```

Refer to your language server's documentation for available options.

### Server Settings

Some language servers request configuration from the client via
`workspace/configuration`. The `settings` table provides these values
on the `[server.*]` entry. The TOML nesting mirrors the JSON object
the server expects — Catenary matches the `section` path from each
request and returns the corresponding subtree.

```toml
[server.pyright.settings.python]
pythonPath = "/usr/bin/python3"

[server.pyright.settings.python.analysis]
exclude = ["**/target", "**/node_modules"]
extraPaths = []
```

When pyright sends `workspace/configuration` with
`{ "items": [{ "section": "python.analysis" }] }`, Catenary returns
the matching subtree from `[server.pyright.settings]`.

Items with no matching path receive `{}`.

### Diagnostic Severity

`min_severity` on `[server.*]` filters which diagnostics are delivered to
agents. Valid values: `"error"`, `"warning"`, `"information"`, `"hint"`.
When absent, all severities are delivered.

```toml
[server.marksman]
command = "marksman"
args = ["server"]
min_severity = "warning"
```

### Environment Variables

`env` on `[server.*]` sets environment variables on the spawned server
process. Variables are added to the inherited environment — if a key
already exists, the config value wins.

```toml
[server.rust-analyzer]
command = "rustup"
args = ["run", "stable", "rust-analyzer"]
env = { CLIPPY_DISABLE_DOCS_LINKS = "1" }
```

Use cases include stripping lint URLs from diagnostics (saves agent
context tokens), setting custom module paths, and passing runtime flags
to language servers that read them from the environment.

### Multi-server Bindings

The `servers` list on `[language.*]` supports multiple servers. List order
defines dispatch priority — for request/response methods, Catenary tries
each server in order and returns the first non-empty result.

```toml
[language.shellscript]
servers = ["termux-ls", "bash-ls"]
```

To suppress diagnostics from a specific server, use inline-table syntax:

```toml
[language.shellscript]
servers = [
    "termux-ls",
    { name = "bash-ls", diagnostics = false },
]
```

Bare strings expand to `{ name = "...", diagnostics = true }`.

To suppress all diagnostics for a language, set `diagnostics = false` on the
language entry:

```toml
[language.markdown]
servers = ["marksman"]
diagnostics = false
```

Precedence: `language.diagnostics AND binding.diagnostics`. Either `false`
suppresses delivery.

| `[language.*].diagnostics` | Per-binding `diagnostics` | Effective |
|---|---|---|
| unset / `true` | unset / `true` | deliver |
| `false` | any | suppress (language-wide) |
| unset / `true` | `false` | suppress (per-server) |

To suppress specific LSP methods from a server for a language binding, use
`disabled_methods`:

```toml
[language.shellscript]
servers = [
    "termux-ls",
    { name = "bash-ls", disabled_methods = ["textDocument/references"] },
]
```

When a method appears in `disabled_methods`, the server is excluded from
dispatch for that method. Other methods (definition, document symbols, etc.)
remain available. Method names use the LSP protocol form.

### Dispatch Filtering

`file_patterns` on `[server.*]` narrows which files a server handles
within its language. Patterns match against the filename (not the full path).
Servers without `file_patterns` handle all files for their language.

```toml
[server.termux-ls]
command = "termux-language-server"
args = ["--stdio"]
file_patterns = ["PKGBUILD", "*.ebuild"]

[server.bash-ls]
command = "bash-language-server"
args = ["start"]

[language.shellscript]
servers = ["termux-ls", "bash-ls"]
```

Here, `termux-ls` only receives PKGBUILD and `*.ebuild` files.
`bash-ls` has no `file_patterns`, so it handles all shellscript files.
For a PKGBUILD file, both servers are active — `termux-ls` is tried
first (higher priority), with `bash-ls` as fallback.

### Single-file Mode

`single_file = true` on `[server.*]` enables tier 3 routing: files
outside all workspace roots get a dedicated server instance with
`rootUri: null` and `workspaceFolders: null` (per the LSP spec's
single-file semantics). The server operates on individual documents
without workspace context.

```toml
[server.bash-ls]
command = "bash-language-server"
args = ["start"]
single_file = true
```

Servers configured with `single_file = true` also track out-of-root
edits through the implicit editing batch, so `catenary diagnostics`
reports errors for files outside the workspace. If the server rejects
null-workspace
initialization at runtime, the failure is cached and the server is not
retried for the remainder of the session.

Default is `false`. Servers that require a project root (Cargo.toml,
tsconfig.json, etc.) should leave this unset.

**Why config-driven, not auto-detected?** The LSP spec allows `rootUri`
to be null, and most servers accept it — but "accepts initialization"
doesn't mean "works well." `rust-analyzer` initialises with null
workspace and enters detached-file mode, but provides heavily degraded
results. `bash-language-server` works fine. There is no LSP capability
flag that distinguishes these cases. Neovim's `nvim-lspconfig` uses the
same approach: a per-server `single_file_support` flag, opt-in, set by
the server config maintainers who know which servers handle it well.

### Suppress SymbolKind

`suppress_symbol_kind = true` on `[server.*]` hides the LSP `SymbolKind`
label (e.g., `Class`, `Struct`, `Module`) in `catenary glob` output.
Useful for non-programming language servers where the symbol name already
embeds structural context — for example, markdown servers that map
headings to `Class` and code blocks to `Object`.

```toml
[server.marksman]
suppress_symbol_kind = true
```

Without suppression:

```
file.md (45 lines)
  :1-5 <Class> H1: Setup Guide
  :8-12 <Struct> Table
```

With `suppress_symbol_kind = true`:

```
file.md (45 lines)
  :1-5 H1: Setup Guide
  :8-12 Table
```

Default is `false`.

### Root Markers

`root_markers` on `[language.*]` defines project boundary files for
sub-root resolution. When a file in a workspace root needs a server
instance that doesn't exist yet, Catenary walks up from the file
toward the workspace root boundary, stopping at the first directory
containing any marker. That directory becomes the server instance's
root.

```toml
[language.rust]
root_markers = ["Cargo.toml"]
```

Entries can be exact filenames or glob patterns (`*`, `?`, `[`). Exact
filenames use a fast `exists()` check; glob patterns are compiled at
config load time and matched against directory entries. This is useful
for ecosystems where project files have varying names:

```toml
[language.csharp]
root_markers = ["*.sln", "*.csproj"]
```

This fixes polyglot repos and monorepos where the workspace root is
broader than what a server needs. For example, a chezmoi dotfiles repo
with Neovim config at `dot_config/nvim/` — lua\_ls rooted at the
chezmoi root never finds `dot_config/nvim/.luarc.json`. With
`root_markers = [".luarc.json"]`, lua\_ls spawns rooted at the
subdirectory and discovers the config.

**Defaults are shipped** for common languages (Rust, Go, Python,
TypeScript, Lua, Java, C/C++, C#, F#, and others) in the builtin
config. Run `catenary doctor <server>` to see active markers. Override
per-language:

```toml
# Custom markers
[language.rust]
root_markers = ["rust-toolchain.toml"]

# Disable markers entirely
[language.python]
root_markers = []
```

**Key behaviors:**

- **Bounded by workspace root.** The walk never escapes above the
  workspace root. Markers subdivide within roots — they don't extend
  beyond them.
- **Nearest wins.** The closest marker to the file is used. Nested
  markers (workspace `Cargo.toml` + crate `Cargo.toml`) resolve to
  the nearest.
- **No marker → workspace root.** Falls back to current behavior when
  no marker exists.
- **Eager/lazy spawn.** If the workspace root itself contains a marker,
  the server spawns at startup. If markers only exist in subdirectories,
  spawn is deferred until a file there is first accessed.
- **Instance isolation.** Files in different marker-resolved sub-roots
  get separate server instances. Files in the same sub-root share one.

### Custom Languages

Define a custom language by adding a `[language.*]` entry with
classification fields and a server binding:

```toml
[language.pkgbuild]
filenames = ["PKGBUILD"]
servers = ["termux-ls"]
```

Classification fields:

- `extensions` — file extensions without the dot (e.g., `["sh", "bash"]`)
- `filenames` — exact filename matches (e.g., `["PKGBUILD", "Makefile"]`)
- `shebangs` — interpreter basenames for `#!` detection (e.g., `["bash", "sh"]`)

Setting a field replaces the default value (if any). Fields not specified
inherit from the default classification. Setting a field to an empty list
clears the default.

Classification precedence (highest first): shebang > filename > extension.

## Project Configuration

Place a `.catenary.toml` in a workspace root to override language and
server configuration and set the per-project build tool for that root.
Supported sections are `lsp`, `[language.*]`, `[server.*]`, and
`[commands]` (the build tool only — command **enforcement** is user-level;
see [Project-scoped commands](#project-scoped-commands)). Other sections
(`[notifications]`, `[icons]`, etc.) are user-level and belong in
`~/.config/catenary/config.toml`.

Project config is discovered when roots are added (at startup or via
`catenary roots add`). Changes to `.catenary.toml` require restarting
the session.

### Disabling Catenary

Set `lsp = false` to turn Catenary off for a workspace:

```toml
# .catenary.toml
lsp = false
```

When the primary workspace root has `lsp = false`, the entire session is
disabled: no tools appear in `tools/list`, no LSP servers spawn, all
hooks pass through, and no database rows are written. The MCP process
still runs (the host starts it) but is invisible to the agent.

This is useful for media collections, documentation repos, data
directories, or any workspace where LSP is pure overhead.

> **Migration:** The old `enabled` key is still accepted with a
> deprecation warning. Rename it to `lsp`. Using both in the same file
> is an error.

### Merge Semantics

Project config is deep-merged with user config at the key level:

- **Scalars replace** — `command`, `args`, `min_severity`.
- **Tables deep-merge by key** — a project `[server.rust-analyzer]` with just
  `settings` inherits `command` and `args` from the user's (or built-in) `[server.rust-analyzer]`.
- **Arrays replace** — `servers`, `file_patterns`, `extensions`,
  `filenames`, `shebangs`.

### Example

Override rust-analyzer settings for a specific project:

```toml
# .catenary.toml (in project root)
[server.rust-analyzer.settings.rust-analyzer]
check.targets = ["aarch64-unknown-linux-gnu"]
cargo.features = ["embedded"]
```

This merges with the built-in (or user-defined) `[server.rust-analyzer]`
definition — the project inherits `command`, `args`, and
`initialization_options`, and overrides only the `settings` subtree.

### Tier Promotion

Adding a `[language.*]` entry in project config promotes that language
to a project-scoped server instance — a separate process bound to this
root. Without a `[language.*]` entry, the shared server instance serves
this root with `scopeUri`-merged settings.

```toml
# .catenary.toml — promotes rust to a project-scoped instance
[language.rust]
servers = ["rust-analyzer"]

[server.rust-analyzer.settings.rust-analyzer]
cargo.features = ["embedded"]
```

## Language IDs

The `[language.<language-id>]` key in the language section must match the LSP language identifier.
Catenary auto-detects languages from file extensions, filenames, and
shebangs (`#!` lines in extensionless scripts). Any language with an LSP
server works — this table covers what Catenary recognises automatically.
To extend or override these defaults, see [Custom Languages](#custom-languages).

### By extension

| Extension | Language ID |
|-----------|-------------|
| `.rs` | `rust` |
| `.go` | `go` |
| `.c` | `c` |
| `.cpp`, `.cc`, `.cxx`, `.h`, `.hpp` | `cpp` |
| `.zig` | `zig` |
| `.d` | `d` |
| `.v` | `v` |
| `.nim` | `nim` |
| `.java` | `java` |
| `.kt`, `.kts` | `kotlin` |
| `.scala`, `.sc` | `scala` |
| `.groovy`, `.gvy` | `groovy` |
| `.clj`, `.cljs`, `.cljc` | `clojure` |
| `.cs` | `csharp` |
| `.fs`, `.fsx`, `.fsi` | `fsharp` |
| `.swift` | `swift` |
| `.m`, `.mm` | `objective-c` |
| `.py` | `python` |
| `.rb` | `ruby` |
| `.pl`, `.pm` | `perl` |
| `.php` | `php` |
| `.lua` | `lua` |
| `.tcl` | `tcl` |
| `.cr` | `crystal` |
| `.js`, `.mjs`, `.cjs` | `javascript` |
| `.ts`, `.mts`, `.cts` | `typescript` |
| `.tsx` | `typescriptreact` |
| `.jsx` | `javascriptreact` |
| `.hs`, `.lhs` | `haskell` |
| `.ml`, `.mli` | `ocaml` |
| `.elm` | `elm` |
| `.gleam` | `gleam` |
| `.ex`, `.exs` | `elixir` |
| `.erl`, `.hrl` | `erlang` |
| `.purs` | `purescript` |
| `.sh`, `.bash`, `.zsh`, `.ebuild`, `.eclass`, `.install` | `shellscript` |
| `.fish` | `fish` |
| `.ps1`, `.psm1`, `.psd1` | `powershell` |
| `.r`, `.R` | `r` |
| `.jl` | `julia` |
| `.mojo` | `mojo` |
| `.html`, `.htm` | `html` |
| `.css` | `css` |
| `.scss` | `scss` |
| `.sass` | `sass` |
| `.less` | `less` |
| `.svelte` | `svelte` |
| `.vue` | `vue` |
| `.json`, `.jsonc` | `json` |
| `.yaml`, `.yml` | `yaml` |
| `.toml` | `toml` |
| `.xml`, `.xsl`, `.xslt`, `.xsd` | `xml` |
| `.sql` | `sql` |
| `.graphql`, `.gql` | `graphql` |
| `.proto` | `proto` |
| `.md`, `.mdx` | `markdown` |
| `.rst` | `restructuredtext` |
| `.tex`, `.latex` | `latex` |
| `.typ` | `typst` |
| `.nix` | `nix` |
| `.tf`, `.tfvars` | `terraform` |
| `.cmake` | `cmake` |
| `.dart` | `dart` |
| `.dockerfile` | `dockerfile` |

### By filename

| Filename | Language ID |
|----------|-------------|
| `Dockerfile` | `dockerfile` |
| `Makefile`, `GNUmakefile` | `makefile` |
| `CMakeLists.txt` | `cmake` |
| `Cargo.toml`, `Cargo.lock` | `toml` |
| `Gemfile`, `Rakefile` | `ruby` |
| `Justfile`, `justfile` | `just` |
| `PKGBUILD` | `shellscript` |

### By shebang

For files without a recognised extension, Catenary reads the first line.
If it starts with `#!`, the interpreter name is matched:

| Interpreter | Language ID |
|-------------|-------------|
| `bash`, `sh`, `zsh`, `dash`, `ksh` | `shellscript` |
| `fish` | `fish` |
| `python`, `python3`, `python2` | `python` |
| `node`, `nodejs` | `javascript` |
| `deno` | `typescript` |
| `ruby`, `irb` | `ruby` |
| `perl` | `perl` |
| `php` | `php` |
| `lua`, `luajit` | `lua` |
| `tclsh`, `wish` | `tcl` |
| `Rscript` | `r` |
| `julia` | `julia` |
| `elixir`, `iex` | `elixir` |
| `erl` | `erlang` |
| `swift` | `swift` |
| `kotlin` | `kotlin` |
| `scala` | `scala` |
| `groovy` | `groovy` |
| `crystal` | `crystal` |

## Command Filtering

The `[commands]` section defines which shell commands an agent may run.
Allowlist-based: only explicitly permitted commands can execute.
Everything else is denied. The denial names the blocked command, shows the
cwd's build tool when the command is build-related, and points the agent at
[`catenary commands`](#inspecting-the-surface) — which prints the active
allow / pipeline / denied surface — so the full configuration lives in one
place instead of being dumped inline on every denial.

### Three states

1. **No `[commands]` section** — not configured yet. Catenary emits a
   hint notification once per session at startup.
2. **`client_enforcement_only = true`** — deliberate opt-out. No hint,
   no enforcement.
3. **`allow = [...]` present** — active allowlist. Enforce.

### Example

```toml
[commands]
build = "make"
# `allow` includes read/stdout-only tools (cat, head, less, diff, ...):
# reads aren't a write vector, and redirected writes (`cat > f`) are
# caught by the redirect gate, not by blocking cat.
allow = ["git", "gh", "cp", "rm", "mkdir", "mv", "touch",
         "chmod", "sleep", "cd", "true", "false", "which",
         "cat", "head", "tail", "less", "more", "diff",
         "echo", "printf", "seq"]
pipeline = ["grep", "wc", "jq", "sort", "tr", "cut", "uniq"]

[commands.deny]
git = ["grep", "ls-files", "ls-tree"]
sqlite3 = ["-cmd"]
```

Read and stdout-only tools (`cat`, `head`, `tail`, `less`, `diff`, …)
live in `allow`, not `pipeline`: reads are not a write vector, and a
redirected write like `cat > f` is caught by the redirect gate (see
`allow_file_redirects` below), so there is no need to block them.
`awk` and `sed` are deliberately **absent** from the default `pipeline` —
both can execute arbitrary code and write files (`sed -i`, `awk`'s
`system()`/`print > file`), which would bypass the tracked Edit/Write
path. Route sweeping edits through `catenary sed` instead.

### Keys

| Key | Description |
|-----|-------------|
| `client_enforcement_only` | Deliberate opt-out — no enforcement, no hint notification. |
| `build` | The project's build tool (e.g., `"make"`). On denial of a build-related command, the response directs the agent to the configured build tool. |
| `allow` | Commands the agent can run unconditionally (including read/stdout-only tools — reads are not a write vector). |
| `pipeline` | Commands allowed mid-pipeline (reading stdin) but denied at pipeline position 0 (reading files directly). Prevents `grep foo bar.rs` while allowing `make test \| grep FAIL`. |
| `deny.<cmd>` | Subcommand denylist within an allowed command. `git` is allowed, but `git grep` is denied. |
| `deny_flags.<cmd>` | Flag denylist within an allowed command. `make` is allowed, but `make -C` is denied. |
| `allow_file_redirects` | Permit `>`/`>>`/`2>file` redirects (default `false`). A redirected write bypasses the tracked Edit/Write path, so the diagnostics batch can be incomplete. fd-dups (`2>&1`, `>&2`) and device sinks (`/dev/null`, …) are always allowed regardless. |
| `guidance.<group>` | Optional per-command hint shown on denial — a `message`, or a `redirect` naming the Catenary command to use instead (`grep` → `catenary grep`, `glob` → `catenary glob`). |

### Inspecting the surface

`catenary commands` prints the active command surface for the current
configuration — the cwd's build tool, then the allow / pipeline / denied
sections, the same `[commands]` rules the `PreToolUse` hook enforces. Run it
(via the host's shell tool) to see what's permitted; denial messages point
here rather than dumping the whole surface inline. The build tool is resolved
for the current directory the same way the denial hint is — the nearest
`.catenary.toml`'s per-root `build`, falling back to the user default — so an
agent that runs `catenary commands` eagerly learns its build tool up front.
It is a stateless read, so it runs even while the command filter is active.

### Project-scoped commands

In `.catenary.toml`, `[commands]` honors **only** `build` — the per-root
build tool ("in this project, the build tool is `make`"). Even disabled
roots (`lsp = false`) contribute `commands.build`. In multi-root sessions
`build` is collected per-root; the evaluator resolves which root a command
targets via `cwd`.

```toml
# .catenary.toml
[commands]
build = "make"
```

**Only `build` is honored.** Every other `[commands]` key —
`client_enforcement_only`, `allow`, `pipeline`, `deny`, `deny_flags`,
`allow_file_redirects`, and `guidance` — is **user-level only** and is
ignored at project scope (Catenary warns when it sees one). They must live
in `~/.config/catenary/config.toml`.

This includes the on/off switch: a project cannot turn enforcement *on*
(`client_enforcement_only = false`) or *off* (`client_enforcement_only =
true`) for itself.

Why: the command filter resolves daemon-globally. One Catenary daemon
serves every connected session, so a project that changed enforcement —
relaxing it (`allow_file_redirects = true`, a wider `allow`) **or** trying
to tighten it (`allow_file_redirects = false`, `client_enforcement_only =
false`) — would change the filter *every* session sees, including agents
in unrelated repos. Worse, the tighten/turn-on direction would fail
*silently*: enforcement never engages, so no agent ever hits a hook to
reveal that the project's request was dropped. Keeping enforcement
user-level makes the filter exactly what the user configured, regardless of
which projects are open. `build` is exempt: it only names a build tool and
relaxes nothing.

Run `catenary config` to generate a recommended config template with
a commented-out `[commands]` section.

## Global Options

| Option | Default | Description |
|--------|---------|-------------|
| `log_retention_days` | `7` | Days to keep dead session data. `0` = remove on startup. `-1` = retain forever. |

## Notifications

The `[notifications]` table controls which tracing events are promoted
to user-facing notifications via the host CLI's `systemMessage`. See
[Notifications](notifications.md) for details on delivery timing, dedup,
and overflow.

```toml
[notifications]
threshold = "warn"    # default
```

| Option | Default | Description |
|--------|---------|-------------|
| `threshold` | `"warn"` | Minimum severity for notification delivery. One of `"debug"`, `"info"`, `"warn"`, `"error"`. |

## Diagnostics

The `[tools]` table tunes `catenary diagnostics` (the command that ends an
edit batch and reports errors and warnings — see
[CLI & Dashboard](cli.md#catenary-diagnostics)).

```toml
[tools]
diagnostics_per_page = 50         # default
diagnostics_severity = "error"    # default
```

| Option | Default | Description |
|--------|---------|-------------|
| `diagnostics_per_page` | `50` | Single-shot preview budget. When a run produces more than this many diagnostics, the preview shows the first N (errors before warnings) and the complete set is written to a per-session file under the runtime dir, named in a trailing "… N more — full report at `<path>`" line. |
| `diagnostics_severity` | `"error"` | Minimum severity that marks a run "dirty" (exit code `1`). One of `"error"`, `"warning"`, `"info"`, `"hint"`. With the default, the exit code means "does it compile" — warnings still print but exit `0`. |

## Icons

The `[icons]` table controls icons in the TUI dashboard.

| Preset | Description |
|--------|-------------|
| `unicode` (default) | Safe symbols for any terminal font. |
| `nerd` | Nerd Font glyphs (requires a [patched font](https://www.nerdfonts.com/)). |

```toml
[icons]
preset = "nerd"
```
