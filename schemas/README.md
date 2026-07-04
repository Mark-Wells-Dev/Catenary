# Catenary config JSON Schemas

Two `schemars`-generated JSON Schemas for Catenary's TOML config files (misc
133). Both are generated from the same serde structs the loader deserializes
(`src/config/`), so they cannot drift from the runtime shape — a byte-for-byte
freshness gate in `src/config/schema.rs` regenerates and compares them on every
`make check`.

| File | Applies to | Generated from |
|---|---|---|
| `config.json` | the user config `~/.config/catenary/config.toml` (and any `CATENARY_CONFIG` file) | `config::parse::RawConfig` |
| `catenary-project.json` | a project-local `.catenary.toml` | the project-scope mirror in `config::schema` |

The project schema is narrowed to the keys Catenary honors at project scope
(`[lsp]`, `[linter]`, `[diagnostics]`, and `[commands].build`); the
user-scope-only enforcement keys (`allow`, `deny_flags`, `allow_flags`,
`script_hosts`, …) are marked `deprecated` so an editor warns exactly where the
runtime warns-and-ignores. The pass-through subtrees
(`initialization_options`, `settings`, `env`) stay open — server-defined content
is never Catenary's to validate.

## Regenerating

```sh
CATENARY_BLESS_SCHEMAS=1 make test T=schemas_are_fresh
```

## Delivery (validator-neutral)

The artifacts are standard JSON Schema; no validator is blessed, each gets a
delivery path:

1. **Published** at the stable docs-site URLs by `.github/workflows/docs.yml`:
   - <https://twowells.github.io/catenary/schemas/config.json>
   - <https://twowells.github.io/catenary/schemas/catenary-project.json>
2. **Auto-associated** at the `taplo` server Catenary spawns
   (`config::schema::install_toml_schema_association`) — a local `file://` copy
   materialized under the cache dir, so the common path is offline.
3. **`#:schema` directive** in the `catenary config` template header, pointing at
   the published `config.json` URL (honored by taplo, tombi, and other
   schema-aware TOML tools).
4. **SchemaStore catalog entry** — prepared below (not yet submitted).

## SchemaStore catalog entry (PREPARED — do not submit from this repo)

To reach VS Code (Even Better TOML), JetBrains, Zed, and Helix with zero
per-editor work, add the following two entries to the `schemas` array of
[`SchemaStore/schemastore`](https://github.com/SchemaStore/schemastore)'s
`src/api/json/catalog.json`, keeping the array in alphabetical order by `name`:

```json
{
  "name": "Catenary configuration",
  "description": "Catenary user configuration (config.toml) — the multi-surface LSP intelligence router.",
  "fileMatch": ["**/catenary/config.toml"],
  "url": "https://twowells.github.io/catenary/schemas/config.json"
},
{
  "name": "Catenary project configuration",
  "description": "Catenary project-local configuration (.catenary.toml).",
  "fileMatch": [".catenary.toml", "**/.catenary.toml"],
  "url": "https://twowells.github.io/catenary/schemas/catenary-project.json"
}
```

### Submission checklist (external — perform manually, later)

1. Confirm the two schema URLs above resolve (the docs-site deploy must have run
   at least once so gh-pages serves `schemas/*.json`).
2. Fork `SchemaStore/schemastore`, add the two entries to
   `src/api/json/catalog.json`, and run their `npm run build` / test task
   locally to validate the catalog.
3. Open a PR against `SchemaStore/schemastore`. The contribution lands
   independently of the Catenary release and pulls the schema from the published
   URL — no schema copy is vendored into SchemaStore.

This is **prepare-only**: nothing here submits anything upstream. Opening the
SchemaStore PR is a deliberate, separate, manual step.
