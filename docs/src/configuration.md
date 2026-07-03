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
[server.lattice]
command = "lattice"
args = ["serve"]
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
servers = ["lattice"]
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
server configuration, set the per-project build tool, and toggle the
diagnostic feeders for that root. Supported keys are the three feeder
toggles (`disable_lsp`, `disable_lint`, `disable_diag`), `[language.*]`,
`[server.*]`, and `[commands]` (the build tool only — command
**enforcement** is user-level; see
[Project-scoped commands](#project-scoped-commands)). Other sections
(`[notifications]`, `[icons]`, etc.) are user-level and belong in
`~/.config/catenary/config.toml`.

Project config is discovered when roots are added (at startup or via
`catenary roots add`). Changes to `.catenary.toml` require restarting
the session.

### Disabling feeders per root

Three orthogonal, per-root toggles control each diagnostic feeder
independently. All default to `false` and are scoped to the root whose
`.catenary.toml` declares them — a multi-project daemon honours each
root's choice separately.

```toml
# .catenary.toml
disable_lsp = true   # no language servers for this root
disable_diag = true  # diagnostics surface off, navigation kept
```

- **`disable_lsp`** — drops the LSP feeder: no language servers spawn for
  this root, so there is no grep/glob enrichment and no LSP diagnostics.
  The root stays tracked everywhere else (`catenary roots ls`, the build
  tool, command resolution, the editing gate). Useful for media
  collections, data directories, or any root where a language server is
  pure overhead. (Polarity flip of the old `lsp = false`.)
- **`disable_lint`** — drops the standalone-linter feeder: no linter
  diagnostics for this root. (No effect yet — the linter framework is not
  shipped; the toggle is parsed and reported by `catenary doctor`.)
- **`disable_diag`** — suppresses the diagnostics **surface** (the
  editing→`catenary diagnostics` gate and its output) while keeping LSP
  servers running for grep/glob navigation. Use it when you want code
  intelligence but no edit-time diagnostics friction.

`disable_lsp` together with `disable_lint` also zeroes diagnostics, but
kills navigation too; `disable_diag` keeps navigation — that is the
distinction.

> **Migration:** The `lsp` key (and its old `enabled` alias) was removed
> in 2.0. Replace `lsp = false` with `disable_lsp = true` — the polarity
> flips. A leftover `lsp`/`enabled` key is now a hard error, flagged by
> `catenary doctor`.

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
# reads aren't a write vector, and a redirected write (`cat > f`) is
# resolved and attributed by the write resolver, not blocked by denying cat.
# `sed` and `perl` are allowed as bulk writers: their in-place edits
# (`sed -i`, `perl -i -pe`) are script-checked and resolved into the
# diagnostics batch; an unparseable/executing script is surgically denied.
allow = ["git", "gh", "cp", "rm", "mkdir", "mv", "touch",
         "chmod", "sleep", "cd", "true", "false", "which",
         "cat", "head", "tail", "less", "more", "diff",
         "echo", "printf", "seq", "sed", "perl"]
pipeline = ["grep", "wc", "jq", "sort", "tr", "cut", "uniq"]

[commands.deny]
git = ["grep", "ls-files", "ls-tree"]
sqlite3 = ["-cmd"]
```

Read and stdout-only tools (`cat`, `head`, `tail`, `less`, `diff`, …)
live in `allow`, not `pipeline`: reads are not a write vector, and a
redirected write like `cat > f` is handled by the [write model](#the-write-model)
— resolved to its target and recorded, or denied when opaque — so there is no
need to block the reader. `awk` and `sed` are deliberately **absent** from the
default `pipeline`, but not because they are banned: their programs are
**checked** by the resolver (a pure `awk` filter or `sed` script passes; an
in-program `system()`/`print > file`, or `sed -i`, resolves to its write-set
or is surgically denied), so keeping them out of the position-0 pipeline simply
avoids masking that check behind a bare `awk 'prog'`.

### Keys

| Key | Description |
|-----|-------------|
| `client_enforcement_only` | Deliberate opt-out — no enforcement, no hint notification. |
| `build` | The project's build tool (e.g., `"make"`). On denial of a build-related command, the response directs the agent to the configured build tool. |
| `allow` | Commands the agent can run unconditionally (including read/stdout-only tools — reads are not a write vector). |
| `pipeline` | Commands allowed mid-pipeline (reading stdin) but denied at pipeline position 0 (reading files directly). Prevents `grep foo bar.rs` while allowing `make test \| grep FAIL`. |
| `deny.<cmd>` | Subcommand denylist within an allowed command. `git` is allowed, but `git grep` is denied. |
| `deny_flags.<cmd>` | Flag denylist within an allowed command. `make` is allowed, but `make -C` is denied. |
| `guidance.<group>` | Optional per-command hint shown on denial — a `message`, or a `redirect` naming the Catenary command to use instead (`grep` → `catenary grep`, `glob` → `catenary glob`). |

### The write model

The `allow`/`pipeline`/`deny` lists above govern which programs may **run**.
How their **writes** are judged is a separate, config-free question:
**resolve-or-deny**. Before a command runs, the `PreToolUse` hook resolves the
complete set of files it will write — from shell grammar (`>`, `>>`, `&>`,
heredoc targets), argument convention (`cp`, `mv`, `tee`, `sed -i`, `ln`),
checkable interpreter programs (`awk`, `perl -pe`'s substitution subset), or a
state query (hook-expanded globs, git asked about its own index). A write whose
target set resolves is **allowed** and recorded into your modified-set, so the
next `catenary diagnostics` sees it. A write whose targets cannot be seen
(`> $DYNAMIC`, `python -c "open(…,'w')"`, `xargs sed -i`) is **denied** with a
message that teaches the resolvable form. fd-dups (`2>&1`, `>&2`) and device
sinks (`/dev/null`, …) are never writes.

This is not a per-user knob — the design decides it (decision 026). There is no
`allow_file_redirects` setting: a resolvable `>` *is* a first-class, tracked
redirect; an opaque one is denied whatever the config says.

### Inspecting the surface

`catenary commands` prints the active command surface for the current
configuration — the cwd's build tool, then the allow / pipeline / denied
sections, and a closing line stating the resolve-or-deny write model — the same
`[commands]` rules the `PreToolUse` hook enforces. Run it
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
and `guidance` — is **user-level only** and is
ignored at project scope (Catenary warns when it sees one). They must live
in `~/.config/catenary/config.toml`.

This includes the on/off switch: a project cannot turn enforcement *on*
(`client_enforcement_only = false`) or *off* (`client_enforcement_only =
true`) for itself.

Why: the command filter resolves daemon-globally. One Catenary daemon
serves every connected session, so a project that changed enforcement —
relaxing it (a wider `allow`) **or** trying to tighten it
(`client_enforcement_only = false` to request enforcement) — would change the
filter *every* session sees, including agents
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

## Companion Roots

The `[roots.companions]` table auto-mounts a **derived sibling root** alongside
each workspace root a host declares — so opening `~/Projects/Catenary` (the code)
also mounts `~/Projects/CatenaryInternal` (the planning repo) for LSP
intelligence, with no manual `catenary roots add` each session.

**Off by default.** Catenary ships no table and assumes no naming convention; an
absent `[roots.companions]` disables the feature entirely.

```toml
[roots.companions]
"*"                  = "{root}Internal"          # any root → its <path>Internal sibling
"~/Projects/homelab" = "~/.local/share/chezmoi"  # explicit, unrelated path
```

Each entry maps a **matcher** (key) to a **companion template** (value):

| Side | Form | Meaning |
|------|------|---------|
| Matcher | `"*"` | Matches any declared root. |
| Matcher | literal path | Matches that one root exactly (after `~`/env expansion). |
| Template | `{root}` | The canonical root path — `"{root}Internal"` appends `Internal`. |
| Template | `{name}` | The root's basename — `"~/docs/{name}"` mounts a cross-parent companion. |
| Template | literal path | A fully explicit companion path. |

`~` and `$VAR`/`${VAR}` expand on **both** sides. Semantics are a **union**, not
first-match: every matching rule contributes a candidate, candidates are
**existence-filtered** (a companion is mounted only if it resolves to an existing
directory), de-duplicated, and never added if it equals a declared root. So
`"*" = "{root}Internal"` is safe to leave on globally — roots without an
`Internal` sibling simply contribute nothing.

**Worktree-aware.** A git worktree's companion derives from its *upstream
project*, not its checkout path: a worktree at
`~/Projects/Worktrees/Catenary-bug24` mounts `~/Projects/CatenaryInternal`, not
`…Catenary-bug24Internal`. The upstream project is found by reading git's own
`.git`/`gitdir`/`commondir` pointer files — no git binary or library is required.
The canonical project root is used only to *derive* the companion; it is never
itself mounted (you keep working in your worktree).

**Lifecycle.** Companions are scoped to the MCP connection that pulled them in.
They are recomputed from the connection's full declared-root set on every change,
so adding a root adds its companion and removing a root drops its companion
automatically. A companion shared by several connections (same project, multiple
agents) stays mounted until the last connection closes, then drops with it.

**User config only.** `[roots.companions]` is read only from your user config
(`~/.config/catenary/config.toml`), never from a project `.catenary.toml` — a
public repository must not be able to leak a private sibling path. A `[roots]`
section placed in a project config is warned about and ignored.

**Why it matters (Lattice synergy).** With markdown defaulting to
[Lattice](lsp/markdown.md), auto-mounting the `*Internal` planning repo lights up
its predicate/backlink intelligence: grep/glob enrichment across the planning
graph, and `catenary diagnostics` link/predicate checks on planning edits — for
free, every session.

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
