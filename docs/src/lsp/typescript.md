# TypeScript

## Install

### macOS

```bash
npm install -g typescript typescript-language-server
```

### Linux

```bash
npm install -g typescript typescript-language-server
```

### Windows

```bash
npm install -g typescript typescript-language-server
```

## Config

Catenary ships a built-in definition for `typescript-ls` — no
`[lsp.server.*]` config is needed. If `typescript-language-server` is on
PATH, it works automatically for TypeScript, JavaScript, TSX, and JSX
files.

## Notes

- The same server handles both TypeScript and JavaScript (see [JavaScript](javascript.md))
- Requires `typescript` as a peer dependency
- Works with `.ts`, `.tsx`, `.mts`, `.cts` files
- Reads your `tsconfig.json` for project settings

## Links

- [typescript-language-server](https://github.com/typescript-language-server/typescript-language-server)
- [TypeScript](https://www.typescriptlang.org/)
