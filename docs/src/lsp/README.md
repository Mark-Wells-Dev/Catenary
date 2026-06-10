# Language Servers

Setup guides for individual language servers. Each page covers installation and
Catenary configuration.

## Languages

| Language(s)        | Page                               | Server                       |
| ------------------ | ---------------------------------- | ---------------------------- |
| CSS, HTML, JSON    | [CSS-HTML-JSON](css-html-json.md)  | vscode-langservers-extracted |
| Go                 | [Go](go.md)                        | gopls                        |
| JavaScript         | [JavaScript](javascript.md)        | typescript-language-server   |
| Julia              | [Julia](julia.md)                  | LanguageServer.jl            |
| Markdown           | [Markdown](markdown.md)            | lattice                      |
| PHP                | [PHP](php.md)                      | intelephense                 |
| Python             | [Python](python.md)                | pyright                      |
| Rust               | [Rust](rust.md)                    | rust-analyzer                |
| Shell (Bash)       | [Shell](shell.md)                  | bash-language-server         |
| Termux & Packaging | [Termux](termux.md)                | termux-language-server       |
| TypeScript         | [TypeScript](typescript.md)        | typescript-language-server   |

## Contributing

Want to add a language?

1. Create `your-language.md` in the `lsp/` folder following the template below
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

Catenary ships a built-in definition for `your-server-name` — no
`[server.*]` config is needed. If `your-server-binary` is on PATH,
it works automatically.

<!-- OR, if the server is not in the built-in defaults: -->

Add to `~/.config/catenary/config.toml`:

```toml
[server.your-language-server]
command = "your-language-server"
args = ["--stdio"]

[language.yourlanguage]
servers = ["your-language-server"]
```

## Notes

Any gotchas, tips, or links to official docs.
````
