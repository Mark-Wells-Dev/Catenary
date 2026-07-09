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
[lsp.server.termux-language-server]
args = ["--stdio"]
file_patterns = ["PKGBUILD", "*.ebuild", "*.eclass"]

# bash-language-server is built-in — no [lsp.server.bash-language-server] needed

[lsp.language.shellscript]
servers = ["termux-language-server", "bash-language-server"]
```

`termux-language-server` handles PKGBUILD and ebuild files with
package-specific intelligence. `bash-language-server` provides shell
fundamentals (definition, references, symbols) for all shellscript files. For
PKGBUILD files, `termux-language-server` is tried first; `bash-language-server`
fills in for methods it doesn't handle. See
[Dispatch Filtering](../configuration.md#dispatch-filtering).

### As a standalone server

For the full set of termux-language-server language IDs, define
each as a [custom language](../configuration.md#custom-languages):

```toml
[lsp.server.termux-language-server]
args = ["--stdio"]

[lsp.language.pkgbuild]
filenames = ["PKGBUILD"]
servers = ["termux-language-server"]

[lsp.language.ebuild]
extensions = ["ebuild"]
servers = ["termux-language-server"]

[lsp.language.eclass]
extensions = ["eclass"]
servers = ["termux-language-server"]
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
