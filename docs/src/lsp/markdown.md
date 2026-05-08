# Markdown

## Install

### macOS

```bash
brew install marksman
```

### Linux

Download the latest binary from
[GitHub releases](https://github.com/artempyanykh/marksman/releases):

```bash
# Example for x86_64
curl -L https://github.com/artempyanykh/marksman/releases/latest/download/marksman-linux-x64 -o marksman
chmod +x marksman
sudo mv marksman /usr/local/bin/
```

### Windows

```powershell
# With Chocolatey
choco install marksman

# Or download from GitHub releases
# https://github.com/artempyanykh/marksman/releases
```

## Config

Catenary ships a built-in definition for `marksman` — no `[server.*]`
config is needed. If `marksman` is on PATH, it works automatically.

## Notes

- Marksman provides document symbols (headings), go-to-definition for
  wiki-links, and references
- Works well with `codebase_map` to show document structure
- Supports wiki-style `[[links]]` and standard `[links](url)`

## Links

- [Marksman GitHub](https://github.com/artempyanykh/marksman)
