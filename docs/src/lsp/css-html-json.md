# CSS, HTML, JSON

These three languages are bundled together in one package: `vscode-langservers-extracted`.

## Install

### macOS

```bash
npm install -g vscode-langservers-extracted
```

### Linux

```bash
npm install -g vscode-langservers-extracted
```

### Windows

```bash
npm install -g vscode-langservers-extracted
```

## Config

Catenary ships built-in definitions for `vscode-css`, `vscode-html`,
and `vscode-json` — no `[server.*]` config is needed. If the binaries
are on PATH, they work automatically for CSS, SCSS, Less, HTML, JSON,
and JSONC files.

## What's Included

The `vscode-langservers-extracted` package provides:

| Server | Languages |
|--------|-----------|
| `vscode-css-language-server` | CSS, SCSS, Less |
| `vscode-html-language-server` | HTML |
| `vscode-json-language-server` | JSON, JSONC |
| `vscode-markdown-language-server` | Markdown |
| `vscode-eslint-language-server` | ESLint |

## Notes

- These servers are extracted from VS Code, so they're well-maintained and feature-complete
- SCSS and Less use the same CSS server — it auto-detects the language
- For Tailwind CSS support, use `tailwindcss-language-server` (separate server)

## Links

- [vscode-langservers-extracted on npm](https://www.npmjs.com/package/vscode-langservers-extracted)
