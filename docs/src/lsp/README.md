# Language Servers

Setup guides for individual language servers. Each page covers installation and
Catenary configuration.

Manual installs are one of two paths. With `[servers] auto_install = true` in
your user config, Catenary installs conformance-vetted servers itself, into a
directory it owns — see
[Managed language servers](../installation.md#managed-language-servers). The
pages here cover the manual system install: use them for servers outside the
vetted set, or when you prefer to manage server binaries yourself.

## Languages

| Language(s)        | Page                               | Server                       |
| ------------------ | ---------------------------------- | ---------------------------- |
| CSS, HTML, JSON    | [CSS-HTML-JSON](css-html-json.md)  | vscode-langservers-extracted |
| Go                 | [Go](go.md)                        | gopls                        |
| JavaScript         | [JavaScript](javascript.md)        | typescript-language-server   |
| Julia              | [Julia](julia.md)                  | LanguageServer.jl            |
| Markdown           | [Markdown](markdown.md)            | lattice                      |
| PHP                | [PHP](php.md)                      | intelephense                 |
| Python             | [Python](python.md)                | pyright-langserver           |
| Rust               | [Rust](rust.md)                    | rust-analyzer                |
| Shell (Bash)       | [Shell](shell.md)                  | bash-language-server         |
| Termux & Packaging | [Termux](termux.md)                | termux-language-server       |
| TypeScript         | [TypeScript](typescript.md)        | typescript-language-server   |

## Contributing

Want to add a language?

1. Create a page for your language in the `lsp/` folder following the template below
2. Add a row to the table above
3. Submit a PR

### Template

````markdown
# YourLanguage

## Install

### macOS

```bash
# install command
```

### Linux

```bash
# install command
```

### Windows

```bash
# install command
```

## Config

Catenary ships a built-in definition for `your-language-server` — no
`[lsp.server.*]` config is needed. If `your-language-server` is on PATH,
it works automatically.

<!-- OR, if the server is not in the built-in defaults: -->

Add to `~/.config/catenary/config.toml`. The `[lsp.server.*]` section key
**is** the server binary Catenary spawns (add `path = "/abs/path"` only to
relocate a binary that is not on PATH):

```toml
[lsp.server.your-language-server]
args = ["--stdio"]

[lsp.language.yourlanguage]
servers = ["your-language-server"]
```

## Notes

Any gotchas, tips, or links to official docs.
````
