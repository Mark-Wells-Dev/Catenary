# Python

## Install

[Pyright](https://github.com/microsoft/pyright) is a fast, feature-rich Python language server from Microsoft.

### macOS

```bash
npm install -g pyright
```

Or via Homebrew:

```bash
brew install pyright
```

### Linux

```bash
npm install -g pyright
```

### Windows

```bash
npm install -g pyright
```

## Config

Catenary ships a built-in definition for pyright — no `[server.*]`
config is needed. If `pyright-langserver` is on PATH, it works
automatically.

To customise settings, add to `~/.config/catenary/config.toml`:

```toml
[server.pyright.settings.python]
pythonPath = "/usr/bin/python3"

[server.pyright.settings.python.analysis]
exclude = ["**/target", "**/node_modules"]
extraPaths = []
```

## Settings

Pyright requests configuration via `workspace/configuration`. Use the `settings`
table on the `[server.*]` entry to provide Python interpreter paths, analysis
exclusions, and other options (shown above).

Without these settings, pyright may fall back to scanning the entire workspace
(including large directories like `target/` or `node_modules/`), which can
cause extremely slow initialization.

See the [Pyright configuration docs](https://github.com/microsoft/pyright/blob/main/docs/configuration.md)
for the full list of available settings.

## Notes

- Pyright provides type checking even for untyped code (infers types)
- Works well with virtual environments — activate your venv before starting your MCP client
- For Django/Flask projects, Pyright handles most patterns out of the box

## Alternatives

### Pylsp (python-lsp-server)

A community-maintained server with plugin support:

```bash
pip install python-lsp-server
```

```toml
[server.pylsp]
command = "pylsp"

[language.python]
servers = ["pylsp"]
```

### Jedi Language Server

Lightweight, uses Jedi for completions:

```bash
pip install jedi-language-server
```

```toml
[server.jedi]
command = "jedi-language-server"

[language.python]
servers = ["jedi"]
```

## Links

- [Pyright](https://github.com/microsoft/pyright)
- [python-lsp-server](https://github.com/python-lsp/python-lsp-server)
