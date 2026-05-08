# Shell (Bash)

## Install

### macOS

```bash
npm install -g bash-language-server
```

### Linux

```bash
npm install -g bash-language-server
```

### Windows

```bash
npm install -g bash-language-server
```

## Config

Catenary ships a built-in definition for `bash-ls` — no `[server.*]`
config is needed. If `bash-language-server` is on PATH, it works
automatically.

## Notes

- The language ID is `shellscript`, not `bash` or `sh`
- Works with `.sh`, `.bash`, `.zsh` files
- Provides completions for commands, variables, and functions
- Integrates with [ShellCheck](https://www.shellcheck.net/) for linting (install separately)

## Multi-server: PKGBUILD Files

For PKGBUILD and other packaging scripts, combine `bash-ls`
with [termux-language-server](termux.md) for enhanced support:

```toml
[server.termux-ls]
command = "termux-language-server"
args = ["--stdio"]
file_patterns = ["PKGBUILD", "*.ebuild"]

[language.shellscript]
servers = ["termux-ls", "bash-ls"]
```

`termux-ls` is tried first for PKGBUILD and ebuild files, with
`bash-ls` filling in for methods termux doesn't handle. See
[Dispatch Filtering](../configuration.md#dispatch-filtering)
for details on `file_patterns`.

## Optional: ShellCheck Integration

For better diagnostics, install ShellCheck:

```bash
# macOS
brew install shellcheck

# Linux (Debian/Ubuntu)
apt install shellcheck

# Linux (Arch)
pacman -S shellcheck
```

The language server will automatically use it if available.

## Links

- [bash-language-server](https://github.com/bash-lsp/bash-language-server)
- [ShellCheck](https://www.shellcheck.net/)
