# Configuration

Catenary loads configuration from multiple sources, in order of priority
(last wins):

1. **Defaults**: `log_retention_days = 7`.
2. **User config**: `~/.config/catenary/config.toml`.
3. **Project config**: `.catenary.toml` in each workspace root. Discovered when roots are added (at startup or via `/add-dir`). Scoped to `[language.*]`, `[server.*]`, and `[commands]` — other sections are user-level.
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

### Example

```toml
[server.rust]
command = "rust-analyzer"

[server.rust.initialization_options]
check.command = "clippy"
cargo.features = "all"
diagnostics.disabled = ["inactive-code"]

[server.python]
command = "pyright-langserver"
args = ["--stdio"]

[server.python.settings.python]
pythonPath = "/usr/bin/python3"

[server.python.settings.python.analysis]
exclude = ["**/target", "**/node_modules"]
extraPaths = []

[server.tsserver]
command = "typescript-language-server"
args = ["--stdio"]

[server.gopls]
command = "gopls"

[language.rust]
servers = ["rust"]

[language.python]
servers = ["python"]

[language.typescript]
servers = ["tsserver"]

[language.go]
servers = ["gopls"]
```

### Initialization Options

Server-specific options passed during the LSP `initialize` request.
These go on the `[server.*]` entry:

```toml
[server.rust.initialization_options]
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
[server.python]
command = "pyright-langserver"
args = ["--stdio"]

[server.python.settings.python]
pythonPath = "/usr/bin/python3"

[server.python.settings.python.analysis]
exclude = ["**/target", "**/node_modules"]
extraPaths = []
```

When pyright sends `workspace/configuration` with
`{ "items": [{ "section": "python.analysis" }] }`, Catenary returns
`{ "exclude": ["**/target", ...], "extraPaths": [] }`.

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

Servers configured with `single_file = true` also gate out-of-root
edits with `start_editing`/`done_editing`, so agents receive diagnostics
for files outside the workspace. If the server rejects null-workspace
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

Place a `.catenary.toml` in a workspace root to override language,
server, and command configuration for that root. Supported sections are
`enabled`, `[language.*]`, `[server.*]`, and `[commands]` — other
sections (`[notifications]`, `[icons]`, etc.) are user-level and belong
in `~/.config/catenary/config.toml`.

Project config is discovered when roots are added (at startup or via
`/add-dir`). Changes to `.catenary.toml` require restarting the session.

### Disabling Catenary

Set `enabled = false` to turn Catenary off for a workspace:

```toml
# .catenary.toml
enabled = false
```

When the primary workspace root has `enabled = false`, the entire session
is disabled: no tools appear in `tools/list`, no LSP servers spawn, all
hooks pass through, and no database rows are written. The MCP process
still runs (the host starts it) but is invisible to the agent.

This is useful for media collections, documentation repos, data
directories, or any workspace where LSP is pure overhead.

### Merge Semantics

Project config is deep-merged with user config at the key level:

- **Scalars replace** — `command`, `args`, `min_severity`.
- **Tables deep-merge by key** — a project `[server.rust]` with just
  `settings` inherits `command` and `args` from the user's `[server.rust]`.
- **Arrays replace** — `servers`, `file_patterns`, `extensions`,
  `filenames`, `shebangs`.

### Example

Override rust-analyzer settings for a specific project:

```toml
# .catenary.toml (in project root)
[server.rust.settings.rust-analyzer]
check.targets = ["aarch64-unknown-linux-gnu"]
cargo.features = ["embedded"]
```

This merges with the user's `[server.rust]` definition — the project
inherits `command`, `args`, and `initialization_options` from user config,
and overrides only the `settings` subtree.

### Tier Promotion

Adding a `[language.*]` entry in project config promotes that language
to a project-scoped server instance — a separate process bound to this
root. Without a `[language.*]` entry, the user's shared server instance
serves this root with `scopeUri`-merged settings.

```toml
# .catenary.toml — promotes rust to a project-scoped instance
[language.rust]
servers = ["rust"]

[server.rust.settings.rust-analyzer]
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
Everything else is denied with a dump of the allowed configuration so
the agent knows its surface area.

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
allow = ["git", "gh", "cp", "rm", "mkdir", "mv", "touch",
         "chmod", "sleep", "cd", "true", "false", "which"]
pipeline = ["grep", "head", "tail", "wc", "jq", "awk",
            "sort", "sed", "tr", "cut", "uniq", "tee"]

[commands.deny]
git = ["grep", "ls-files", "ls-tree"]
sqlite3 = ["-cmd"]
```

### Keys

| Key | Description |
|-----|-------------|
| `client_enforcement_only` | Deliberate opt-out — no enforcement, no hint notification. |
| `build` | The project's build tool (e.g., `"make"`). On denial of a build-related command, the response directs the agent to the configured build tool. |
| `allow` | Commands the agent can run unconditionally. |
| `pipeline` | Commands allowed mid-pipeline (reading stdin) but denied at pipeline position 0 (reading files directly). Prevents `grep foo bar.rs` while allowing `make test \| grep FAIL`. |
| `deny.<cmd>` | Subcommand denylist within an allowed command. `git` is allowed, but `git grep` is denied. |

### Project-scoped commands

In `.catenary.toml`, `[commands]` supports two fields:

- **`build`** — per-root build tool. "In this project, the build tool
  is `make`." Even disabled roots (`enabled = false`) contribute
  `commands.build`.
- **`allow`** — replaces (not merges with) the user's `allow` list for
  this root's contribution.

In multi-root sessions, `allow` lists are unioned across roots — adding
a root is an intentional scope expansion. `build` is collected per-root;
the evaluator resolves which root a command targets via `cwd`.

```toml
# .catenary.toml
[commands]
build = "make"
```

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
