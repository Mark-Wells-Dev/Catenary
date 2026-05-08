# Termux & Packaging

The `termux-language-server` provides advanced support for specialized shell scripts used in Termux, Arch Linux (PKGBUILD), Gentoo (ebuild), and Debian development.

## Install

### macOS

```bash
pip install termux-language-server
```

### Linux

```bash
pip install termux-language-server
```

### Windows

```bash
pip install termux-language-server
```

## Config

### As a supplementary server (recommended)

Use `file_patterns` to add `termux-language-server` alongside
[bash-language-server](shell.md) for packaging files:

```toml
[server.termux-ls]
command = "termux-language-server"
args = ["--stdio"]
file_patterns = ["PKGBUILD", "*.ebuild", "*.eclass"]

# bash-ls is built-in — no [server.bash-ls] needed

[language.shellscript]
servers = ["termux-ls", "bash-ls"]
```

`termux-ls` handles PKGBUILD and ebuild files with package-specific
intelligence. `bash-ls` provides shell fundamentals (definition,
references, symbols) for all shellscript files. For PKGBUILD files,
`termux-ls` is tried first; `bash-ls` fills in for methods it doesn't
handle. See [Dispatch Filtering](../configuration.md#dispatch-filtering).

### As a standalone server

For the full set of termux-language-server language IDs, define
each as a [custom language](../configuration.md#custom-languages):

```toml
[server.termux-ls]
command = "termux-language-server"
args = ["--stdio"]

[language.pkgbuild]
filenames = ["PKGBUILD"]
servers = ["termux-ls"]

[language.ebuild]
extensions = ["ebuild"]
servers = ["termux-ls"]

[language.eclass]
extensions = ["eclass"]
servers = ["termux-ls"]
```

Add entries for other language IDs as needed (`termux`, `makepkg`,
`devscripts`, `mdd`, `subpackage`, `install`, `gentoo-make-conf`,
`make.conf`, `color.map`).

## Notes

- This server is specifically designed for packaging and system-level shell scripts
- Extends the features of `bash-language-server` for specialized formats
- Supports file types like `PKGBUILD`, `build.sh`, `*.ebuild`, and `*.mdd`

## Links

- [termux-language-server GitHub](https://github.com/termux/termux-language-server)
