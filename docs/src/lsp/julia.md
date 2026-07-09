# Julia

## Install

### macOS / Linux / Windows

From the Julia REPL:

```julia
using Pkg
Pkg.add("LanguageServer")
```

## Config

Catenary ships a built-in definition for `julia` — no `[lsp.server.*]`
config is needed. If `julia` is on PATH and `LanguageServer.jl` is
installed, it works automatically.

## Notes

- The server starts a Julia process, which has some startup time
- The built-in uses `--startup-file=no` and `--history-file=no` for faster startup
- First run on a project may take time to load packages and index
- Works best with projects that have a `Project.toml`

## Reducing Startup Time

For faster startup, you can create a custom sysimage:

```julia
using PackageCompiler
create_sysimage([:LanguageServer], sysimage_path="languageserver.so")
```

Then override the built-in default:

```toml
[lsp.server.julia]
args = ["--sysimage=/path/to/languageserver.so", "-e", "using LanguageServer; runserver()"]
```

## Links

- [LanguageServer.jl](https://github.com/julia-vscode/LanguageServer.jl)
- [Julia](https://julialang.org/)
