# JavaScript

JavaScript uses the same language server as TypeScript.

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
`[server.*]` config is needed. If `typescript-language-server` is on
PATH, it works automatically.

## Notes

- Same server as [TypeScript](typescript.md) — install once, configure both
- Works with `.js`, `.jsx`, `.mjs`, `.cjs` files
- Provides type inference even in plain JavaScript
- Add a `jsconfig.json` to customize project settings

## JSX / React

JSX is handled automatically. The built-in defaults bind `typescript-ls`
to `javascriptreact` for `.jsx` files.

## Links

- [typescript-language-server](https://github.com/typescript-language-server/typescript-language-server)
