// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary MCP server and CLI.
//!
//! This is the main entry point for the Catenary multiplexing bridge.
//! It can be run as an MCP server or as a CLI tool to list and monitor sessions.

#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use anyhow::{Context, Result};
use clap::{FromArgMatches, Parser, Subcommand};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use catenary_mcp::cli::{self, HostFormat, QueryFormat};
use catenary_mcp::logging::LoggingServer;

use catenary_mcp::source::Source;

/// Command-line arguments for Catenary.
#[derive(Parser, Debug)]
#[command(name = "catenary")]
#[command(about = "LSP-powered code intelligence for AI agents")]
#[command(version = env!("CATENARY_VERSION"))]
struct Args {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands supported by Catenary.
#[derive(Subcommand, Debug)]
enum Command {
    /// Overview of Catenary's workflow and commands.
    Primer,

    /// Search for a pattern with LSP-enriched results.
    ///
    /// Searches from the current working directory. Results within tracked
    /// workspace roots include symbol context from LSP servers.
    Grep {
        /// Regex pattern (Rust `regex` syntax, | for alternation; no look-around —
        /// unlike `catenary sed`, grep runs the linear ripgrep engine).
        pattern: String,

        /// File or directory path(s) to scope the search.
        ///
        /// Multiple values are unioned — `src/tui/*` expands
        /// to individual files and all are searched.
        #[arg(name = "PATH")]
        scope: Vec<String>,

        /// Exclude matches by glob pattern (e.g., tests/**).
        #[arg(long = "exclude-pattern")]
        exclude: Option<String>,

        /// Page number for paged results.
        #[arg(long, default_value = "1")]
        page: usize,

        /// Report the match count ("N matches in M files") instead of results.
        #[arg(long)]
        count: bool,

        /// Include files ignored by .gitignore.
        #[arg(long)]
        include_gitignored: bool,

        /// Include hidden files and directories.
        #[arg(long)]
        include_hidden: bool,
    },

    /// Browse the filesystem: file outlines, directory listings.
    ///
    /// Resolves against the current working directory. Results include symbol
    /// outlines when LSP data is available.
    Glob {
        /// File or directory path(s).
        ///
        /// Multiple values are unioned — `src/tui/*` expands
        /// to individual files and all are browsed.
        #[arg(name = "PATH", required = true)]
        paths: Vec<String>,

        /// Exclude matches by glob pattern (e.g., tests/**).
        #[arg(long = "exclude-pattern")]
        exclude: Option<String>,

        /// Page number for paged results.
        #[arg(long, default_value = "1")]
        page: usize,

        /// Report the path count ("N paths") instead of results.
        #[arg(long)]
        count: bool,

        /// Include files ignored by .gitignore.
        #[arg(long)]
        include_gitignored: bool,

        /// Include hidden files and directories.
        #[arg(long)]
        include_hidden: bool,
    },

    /// Regex find-and-replace across files (the tracked mass-edit surface).
    ///
    /// Previews by default (resolved file list + per-file match counts, writes
    /// nothing); `--in-place` applies the edits and folds the changed files into
    /// the diagnostics batch — run `catenary diagnostics` after. Capture groups
    /// are `$1` (not `\1`); `\n`/`\t`/`\r` are interpreted. Quote glob patterns
    /// so Catenary expands them gitignore-aware. Invoke via the host's shell tool.
    Sed {
        /// Regex pattern: Rust `regex` syntax plus look-around and
        /// back-references (`Dft(?!Norm)`), | for alternation.
        pattern: String,

        /// Replacement text. $1 references a capture group, $0 the whole match,
        /// $$ a literal $; \n/\t/\r are interpreted. (`&` is a literal `&`.)
        /// Single pass — replacement text is never re-scanned for further matches.
        replacement: String,

        /// File or directory path(s), or quoted glob pattern(s).
        ///
        /// Required — `catenary sed` never rewrites the whole tree implicitly.
        #[arg(name = "PATH")]
        paths: Vec<String>,

        /// Apply the edits (default: preview — shows files + match counts).
        #[arg(long)]
        in_place: bool,

        /// Case-insensitive matching.
        #[arg(long)]
        ignore_case: bool,

        /// Case the replacement to match each hit (Omni→Lattice, omni→lattice).
        #[arg(long)]
        preserve_case: bool,

        /// Replace only the first match per file (default: all).
        #[arg(long)]
        first: bool,

        /// Exclude matches by glob pattern (e.g., tests/**).
        #[arg(long = "exclude-pattern")]
        exclude: Option<String>,

        /// Include files ignored by .gitignore.
        #[arg(long)]
        include_gitignored: bool,

        /// Include hidden files and directories.
        #[arg(long)]
        include_hidden: bool,

        /// Page number for the paged preview.
        #[arg(long, default_value = "1")]
        page: usize,
    },

    /// Print diagnostics for the files you've edited, then clear the set.
    ///
    /// Runs the LSP diagnostics pipeline over every file edited since the
    /// last run: prints errors and warnings (or `[clean]` when none), then
    /// resets so the next edit starts a fresh set. Editing begins implicitly
    /// on the first edit — there is no separate start step. Invoke via the
    /// host's shell tool.
    Diagnostics,

    /// Editing mode (start). Optional — editing starts implicitly on the
    /// first edit; `catenary diagnostics` ends it and prints diagnostics.
    Editing {
        #[command(subcommand)]
        command: EditingCommand,
    },

    /// Workspace root management (add, rm, ls).
    Roots {
        #[command(subcommand)]
        command: RootsCommand,
    },

    /// Output a recommended annotated config template.
    Config,

    /// Print the allowed-command surface (allow / pipeline / denied) the
    /// command filter enforces. Invoke via the host's shell tool to see which
    /// shell commands Catenary permits; the denial message points here.
    Commands,

    /// Check language server health. Tests all configured servers by default.
    /// Pass a server name for verbose single-server diagnostics.
    Doctor {
        /// Server name for verbose single-server mode (matches [server.*]
        /// config keys). When omitted, tests all servers with one-line
        /// summaries.
        server: Option<String>,

        /// Workspace root for the probe and `.catenary.toml` project
        /// config lookup. Defaults to the current working directory.
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Disable colored output.
        #[arg(long)]
        nocolor: bool,

        /// Show a unified diff for every stale file (hooks.json and constrained-bash.py).
        #[arg(long)]
        diff: bool,
    },

    /// Install or update the Catenary plugin for a host CLI.
    Install {
        #[command(subcommand)]
        host: Option<InstallHost>,

        /// Show what would change without acting.
        #[arg(long, global = true)]
        dry_run: bool,
    },

    /// Self-update the Catenary binary from GitHub releases.
    Update {
        /// Print whether an update is available without downloading.
        #[arg(long)]
        check: bool,

        /// Re-download even if versions match.
        #[arg(long)]
        force: bool,
    },

    /// Stop the running Catenary daemon.
    Stop,

    /// Query the JSONL telemetry firehose (LSP/MCP/hook protocol + trace).
    ///
    /// Reads the append-only logs directly, so it works even when the daemon
    /// is down. Filters select which shards to read — `--session` / `--server`
    /// / `--tool` resolve by file name, `--cwd` / `--level` / `--kind` /
    /// `--search` filter records after open. `--follow` tails the selection
    /// live.
    Query {
        /// Filter by session id (or prefix) → that session's file.
        #[arg(long)]
        session: Option<String>,

        /// Filter by LSP server name (rootless file + all rootful instances).
        #[arg(long)]
        server: Option<String>,

        /// Filter by search tool invocation dir ("grep" or "glob").
        #[arg(long)]
        tool: Option<String>,

        /// Keep only records whose recorded cwd is this path or under it.
        #[arg(long)]
        cwd: Option<String>,

        /// Time filter (e.g., "1h", "today", "7d", "30m").
        #[arg(long)]
        since: Option<String>,

        /// Read a specific daemon instance dir (default: the freshest one).
        #[arg(long)]
        instance: Option<String>,

        /// Read every instance dir, not just the freshest one.
        #[arg(long)]
        all_instances: bool,

        /// Minimum severity to show (error, warn, info, debug).
        #[arg(long)]
        level: Option<String>,

        /// Filter by record kind (lsp, mcp, hook, internal).
        #[arg(long)]
        kind: Option<String>,

        /// Free-text substring over method, message, and payload.
        #[arg(long)]
        search: Option<String>,

        /// Live-tail the selected files instead of a one-shot read.
        #[arg(long)]
        follow: bool,

        /// Maximum rows to show (0 = unlimited).
        #[arg(long, default_value = "100")]
        limit: usize,

        /// Output format.
        #[arg(long, value_enum, default_value = "table")]
        format: QueryFormat,
    },

    /// Hook subcommands (invoked by host CLI hooks).
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },

    /// Run as the Catenary daemon (internal, spawned by bridge proxy).
    #[command(hide = true)]
    Daemon,
}

/// Editing mode subcommands.
#[derive(Subcommand, Debug)]
enum EditingCommand {
    /// Enter editing mode (optional). Editing now starts implicitly on the
    /// first edit; this remains as an idempotent confirmation so a stray
    /// invocation never errors. Invoke via the host's shell tool.
    Start,
}

/// Workspace root management subcommands.
#[derive(Subcommand, Debug)]
enum RootsCommand {
    /// Add a workspace root.
    Add {
        /// Path to add as a workspace root.
        path: PathBuf,
    },
    /// Remove a workspace root.
    Rm {
        /// Path to remove from workspace roots.
        path: PathBuf,
    },
    /// List all tracked workspace roots with their source.
    Ls,
}

/// Hook subcommands invoked by host CLI hooks.
#[derive(Subcommand, Debug)]
enum HookCommand {
    /// Pre-agent: signal turn start (`UserPromptSubmit` / `BeforeAgent`).
    #[command(name = "pre-agent")]
    PreAgent {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// Pre-tool: editing state enforcement (`PreToolUse` / `BeforeTool`).
    #[command(name = "pre-tool")]
    PreTool {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// Post-agent: force `done_editing` before agent finishes (`Stop` / `AfterAgent`).
    #[command(name = "post-agent")]
    PostAgent {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `SessionStart`: clear stale editing state.
    #[command(name = "session-start")]
    SessionStart {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `SessionEnd`: clean up session state (roots, editing).
    #[command(name = "session-end")]
    SessionEnd {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `SubagentStart`: mount the subagent's worktree as a root.
    #[command(name = "subagent-start")]
    SubagentStart {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `WorktreeRemove`: tear down the subagent's worktree root.
    #[command(name = "worktree-remove")]
    WorktreeRemove {
        /// Output format: "claude", "gemini", or "antigravity".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
}

/// Host targets for the install command.
#[derive(Subcommand, Debug)]
enum InstallHost {
    /// Install the Catenary plugin for Claude Code.
    Claude {
        /// Source: local path (dev install) or repo identifier (release install).
        source: Option<String>,
    },
    /// Install the Catenary extension for Gemini CLI.
    Gemini {
        /// Source: local path (dev install) or repo identifier (release install).
        source: Option<String>,
    },
    /// Install the Catenary plugin for Antigravity CLI.
    Antigravity {
        /// Source: local path (dev install) or repo identifier (release install).
        source: Option<String>,
    },
    /// Install the Catenary plugin for OpenCode.
    #[command(name = "opencode")]
    OpenCode {
        /// Source: local path (dev install) or repo identifier (release install).
        source: Option<String>,

        /// Install into the current workspace (`<root>/.opencode/`) instead of
        /// the global `~/.config/opencode/` location.
        #[arg(long)]
        workspace: bool,
    },
}

/// Entry point for the Catenary binary.
///
/// Subcommands that need async (Doctor, the in-process MCP server on
/// non-Unix) build a standard tokio runtime on demand. The daemon
/// builds its own runtime with larger thread stacks to accommodate its
/// async state machines. The bridge proxy is entirely synchronous.
#[allow(clippy::too_many_lines, reason = "Dispatch table for all subcommands")]
fn main() -> Result<()> {
    use clap::CommandFactory;
    let cli = Args::command();
    cli::command_filter::set_cli_command(cli.clone());
    let matches = match cli.clone().try_get_matches() {
        Ok(m) => m,
        Err(e) => {
            // Let clap handle --help and --version normally (exit 0).
            if matches!(
                e.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                e.exit();
            }
            // For agent-facing subcommands, append `-h` output so the
            // agent sees correct usage without a second round-trip.
            let raw = e.to_string();
            let subcommand = ["grep", "glob", "sed", "diagnostics", "editing", "roots"]
                .into_iter()
                .find(|cmd| raw.contains(&format!("catenary {cmd}")));
            if let Some(cmd) = subcommand
                && let Some(sub) = cli.find_subcommand(cmd)
            {
                let mut sub = sub.clone();
                sub = sub
                    .bin_name(format!("catenary {cmd}"))
                    .disable_help_subcommand(true);
                let help = sub.render_help().to_string();
                // Extract just the "error:" line, drop clap's tip/Usage/--help boilerplate.
                let error_line = raw
                    .lines()
                    .find(|l| l.starts_with("error:"))
                    .unwrap_or(&raw);
                eprint!("{error_line}\n\n{help}");
                std::process::exit(2);
            }
            e.exit();
        }
    };
    let args = Args::from_arg_matches(&matches).context("parse CLI arguments")?;

    match args.command {
        None => {
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                run_dashboard()
            } else {
                #[cfg(unix)]
                {
                    catenary_mcp::router::run_bridge()
                }
                #[cfg(not(unix))]
                {
                    Err(anyhow::anyhow!(
                        "daemon mode requires Unix — Windows support is planned"
                    ))
                }
            }
        }
        Some(Command::Primer) => {
            let mut out = cli::Output::stdout(false);
            run_primer(&mut out);
            Ok(())
        }
        #[cfg(unix)]
        Some(Command::Grep {
            pattern,
            scope,
            exclude,
            page,
            count,
            include_gitignored,
            include_hidden,
        }) => {
            let paths = to_literal_paths(scope);
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_grep(
                &mut out,
                pattern,
                paths,
                exclude,
                page,
                count,
                include_gitignored,
                include_hidden,
            ))
        }
        #[cfg(not(unix))]
        Some(Command::Grep { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Glob {
            paths,
            exclude,
            page,
            count,
            include_gitignored,
            include_hidden,
        }) => {
            let paths = to_literal_paths(paths);
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_glob(
                &mut out,
                paths,
                exclude,
                page,
                count,
                include_gitignored,
                include_hidden,
            ))
        }
        #[cfg(not(unix))]
        Some(Command::Glob { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Sed {
            pattern,
            replacement,
            paths,
            in_place,
            ignore_case,
            preserve_case,
            first,
            exclude,
            include_gitignored,
            include_hidden,
            page,
        }) => {
            let paths = to_literal_paths(paths);
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_sed(
                &mut out,
                pattern,
                replacement,
                paths,
                in_place,
                ignore_case,
                preserve_case,
                first,
                exclude,
                include_gitignored,
                include_hidden,
                page,
            ))
        }
        #[cfg(not(unix))]
        Some(Command::Sed { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Diagnostics) => {
            let mut out = cli::Output::stdout(false);
            // Exit-code contract (ticket 11): 0 clean / 1 dirty / 2 fault. A
            // fault (no daemon, IPC failure, malformed response) surfaces as
            // `Err` and is distinct from a dirty `1` so the agent can tell
            // "found errors" from "the tool broke".
            match build_runtime().and_then(|rt| rt.block_on(run_done_editing(&mut out))) {
                Ok(DiagnosticsExit::Clean) => Ok(()),
                Ok(DiagnosticsExit::Dirty) => std::process::exit(1),
                Err(e) => {
                    eprintln!("{e:#}");
                    std::process::exit(2);
                }
            }
        }
        #[cfg(not(unix))]
        Some(Command::Diagnostics) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Editing { command }) => {
            let mut out = cli::Output::stdout(false);
            let EditingCommand::Start = command;
            build_runtime()?.block_on(run_start_editing(&mut out))
        }
        #[cfg(not(unix))]
        Some(Command::Editing { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Roots { command }) => {
            let mut out = cli::Output::stdout(false);
            match command {
                RootsCommand::Add { path } => {
                    build_runtime()?.block_on(run_root_command(&mut out, path, "tool/roots-add"))
                }
                RootsCommand::Rm { path } => {
                    build_runtime()?.block_on(run_root_command(&mut out, path, "tool/roots-rm"))
                }
                RootsCommand::Ls => {
                    build_runtime()?.block_on(cli::commands::run_ls_roots(&mut out))
                }
            }
        }
        #[cfg(not(unix))]
        Some(Command::Roots { .. }) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        Some(Command::Config) => {
            let mut out = cli::Output::stdout(false);
            cli::config_template::print_template(&mut out);
            Ok(())
        }
        Some(Command::Commands) => {
            let mut out = cli::Output::stdout(false);
            cli::commands::run_commands(&mut out)
        }
        Some(Command::Doctor {
            server,
            root,
            nocolor,
            diff,
        }) => {
            let rt = build_runtime()?;
            let mut out = cli::Output::stdout(nocolor);
            if let Some(server_name) = server {
                rt.block_on(cli::doctor::run_doctor_single(
                    &mut out,
                    &server_name,
                    &root,
                ))
            } else {
                rt.block_on(cli::doctor::run_doctor(&mut out, &root, diff))
            }
        }
        Some(Command::Install { host, dry_run }) => {
            let mut out = cli::Output::stdout(false);
            match host {
                None => cli::install::run_install_list(&mut out),
                Some(InstallHost::Claude { source }) => {
                    cli::install::run_install_claude(&mut out, source.as_deref(), dry_run)
                }
                Some(InstallHost::Gemini { source }) => {
                    cli::install::run_install_gemini(&mut out, source.as_deref(), dry_run)
                }
                Some(InstallHost::Antigravity { source }) => {
                    cli::install::run_install_antigravity(&mut out, source.as_deref(), dry_run)
                }
                Some(InstallHost::OpenCode { source, workspace }) => {
                    cli::install::run_install_opencode(
                        &mut out,
                        source.as_deref(),
                        workspace,
                        dry_run,
                    )
                }
            }
        }
        Some(Command::Update { check, force }) => {
            let mut out = cli::Output::stdout(false);
            cli::update::run_update(&mut out, check, force)
        }
        #[cfg(unix)]
        Some(Command::Stop) => {
            let mut out = cli::Output::stdout(false);
            build_runtime()?.block_on(run_stop(&mut out))
        }
        #[cfg(not(unix))]
        Some(Command::Stop) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        Some(Command::Query {
            session,
            server,
            tool,
            cwd,
            since,
            instance,
            all_instances,
            level,
            kind,
            search,
            follow,
            limit,
            format,
        }) => {
            let mut out = cli::Output::stdout(false);
            cli::commands::run_query(
                &mut out,
                &cli::commands::QueryArgs {
                    session: session.as_deref(),
                    server: server.as_deref(),
                    tool: tool.as_deref(),
                    cwd: cwd.as_deref(),
                    since: since.as_deref(),
                    instance: instance.as_deref(),
                    all_instances,
                    level: level.as_deref(),
                    kind: kind.as_deref(),
                    search: search.as_deref(),
                    follow,
                    limit,
                    format,
                },
            )
        }
        Some(Command::Hook { command }) => {
            // Install minimal tracing subscriber for hook CLI: only the
            // desktop notification sink. When the daemon is unreachable,
            // error!() events fire OS notifications directly from the
            // hook process.
            let hook_logging = LoggingServer::new();
            tracing_subscriber::registry()
                .with(hook_logging.clone())
                .init();
            let desktop_sink = catenary_mcp::notify::DesktopNotificationSink::new();
            hook_logging.activate(vec![desktop_sink]);

            match command {
                HookCommand::PreAgent { format } => cli::hooks::run_pre_agent(format),
                HookCommand::PreTool { format } => cli::hooks::run_pre_tool(format),
                HookCommand::PostAgent { format } => cli::hooks::run_post_agent(format),
                HookCommand::SessionStart { format } => cli::hooks::run_session_start(format),
                HookCommand::SessionEnd { format } => cli::hooks::run_session_end(format),
                HookCommand::SubagentStart { format } => cli::hooks::run_subagent_start(format),
                HookCommand::WorktreeRemove { format } => cli::hooks::run_worktree_remove(format),
            }
            Ok(())
        }
        #[cfg(unix)]
        Some(Command::Daemon) => run_daemon(),
        #[cfg(not(unix))]
        Some(Command::Daemon) => Err(anyhow::anyhow!("daemon mode requires Unix")),
    }
}

/// Converts positional arguments to `PathBuf`s.
///
/// A plain string→path mapping; literal-vs-pattern classification and glob
/// resolution happen later in [`resolve_search_paths`].
fn to_literal_paths(values: Vec<String>) -> Vec<PathBuf> {
    values.into_iter().map(PathBuf::from).collect()
}

/// Returns `true` if the string contains glob metacharacters (`*`, `?`, `[`, `{`).
fn contains_glob_metachar(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
}

/// Classified search-path arguments under the literal-first contract.
///
/// Produced by [`resolve_search_paths`] and consumed by
/// [`render_search_outcome`] to honor the three-outcome, always-exit-0
/// contract for `catenary grep`/`glob` (`bugs/13`).
struct SearchPaths {
    /// Arguments forwarded to the daemon: paths that exist plus glob patterns
    /// (original spelling). On a zero-result search these are echoed back so
    /// the agent sees exactly what was searched.
    forward: Vec<PathBuf>,
    /// Plain-path arguments that do not exist, reported loudly as
    /// `path does not exist: <path>`.
    missing: Vec<String>,
}

/// Resolves each search-path argument under the literal-first contract.
///
/// For each argument (resolved against `cwd` for the existence probe):
/// - **exists** (file, directory, or symlink — including a broken one) →
///   forwarded as a concrete path. Probing existence *first* preserves
///   literal matching of filenames that contain glob metacharacters and keeps
///   each variadic argument independent.
/// - **missing + glob metacharacter** (`* ? [ {`) → forwarded as a pattern;
///   the daemon expands it via its gitignore-aware walker.
/// - **missing + no metacharacter** → recorded in `missing` for a loud
///   `path does not exist` (exit 0, never a hard error that would cancel a
///   parallel tool batch).
fn resolve_search_paths(args: &[PathBuf], cwd: &Path) -> SearchPaths {
    let mut forward = Vec::new();
    let mut missing = Vec::new();
    for arg in args {
        let resolved = if arg.is_absolute() {
            arg.clone()
        } else {
            cwd.join(arg)
        };
        if resolved.symlink_metadata().is_ok() || contains_glob_metachar(&arg.to_string_lossy()) {
            forward.push(arg.clone());
        } else {
            missing.push(arg.to_string_lossy().into_owned());
        }
    }
    SearchPaths { forward, missing }
}

/// Which search command is being rendered — selects the zero-result wording.
enum SearchKind {
    /// `catenary grep`. On no match, echoes the pattern (so the agent can
    /// check its escaping) and the searched scope.
    Grep {
        /// The search pattern, echoed on a zero-result search.
        pattern: String,
        /// The pattern contained `\|`, a basic-regex alternation that ripgrep
        /// reads as a literal pipe — nudge toward `|`.
        bre_alternation: bool,
    },
    /// `catenary glob`.
    Glob,
}

/// Compresses a path by replacing the `$HOME` prefix with `~`.
fn compress_home(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME")
        && let Ok(rel) = path.strip_prefix(&home)
    {
        return format!("~/{}", rel.display());
    }
    path.display().to_string()
}

/// Joins the forwarded path arguments for the `searched:` echo line.
fn forward_display(paths: &SearchPaths) -> String {
    paths
        .forward
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders a search command's outcome under the three-outcome, always-exit-0
/// contract (`bugs/13`).
///
/// - **Results** — `daemon_output` non-empty → printed verbatim; the daemon
///   prepends its own cwd/root anchor.
/// - **Empty** — `queried` ran but nothing came back → the cwd anchor is
///   always printed (the only signal distinguishing "ran here, found nothing"
///   from "did not run"), then the zero-result echo: `no files matched:
///   <arg>` per glob argument, or grep's `no matches for: <pattern>` plus the
///   `searched:` scope so the agent can check both its escaping and its paths.
/// - **Missing** — a loud `path does not exist: <path>` is appended for each
///   non-existent plain-path argument, regardless of the above.
fn render_search_outcome(
    out: &mut cli::Output,
    cwd: &Path,
    paths: &SearchPaths,
    daemon_output: &str,
    queried: bool,
    kind: &SearchKind,
) {
    let body = daemon_output.trim_end_matches('\n');
    if body.is_empty() {
        let _ = out.writeln(format_args!("cwd: {}", compress_home(cwd)));
        if queried {
            match kind {
                SearchKind::Glob => {
                    for arg in &paths.forward {
                        let _ = out.writeln(format_args!("no files matched: {}", arg.display()));
                    }
                }
                SearchKind::Grep {
                    pattern,
                    bre_alternation,
                } => {
                    let _ = out.writeln(format_args!("no matches for: {pattern}"));
                    if !paths.forward.is_empty() {
                        let _ = out.writeln(format_args!("searched: {}", forward_display(paths)));
                    }
                    if *bre_alternation {
                        let _ = out.writeln(format_args!(
                            "hint: use `|` for alternation, not `\\|` (which matches a literal pipe)"
                        ));
                    }
                }
            }
        }
    } else {
        let _ = out.writeln(format_args!("{body}"));
    }
    for path in &paths.missing {
        let _ = out.writeln(format_args!("path does not exist: {path}"));
    }
}

/// Hand-written workflow preamble, emitted above the auto-generated
/// command reference in [`run_primer`].
///
/// These are the *invariants* — the edit→diagnostics loop, the pull model,
/// deny-as-guidance, bare canonical form, and the navigation model. They are
/// stable and rarely change. The per-command reference below them
/// regenerates from clap help, so adding a flag or command never touches
/// this text.
const PRIMER_PREAMBLE: &str = "\
Catenary — LSP-powered code intelligence, driven from the shell.

Catenary manages a pool of language servers and exposes them through the
commands below. Read these invariants first; the per-command reference
follows.

The edit→diagnostics loop
  Editing is tracked automatically — the first edit starts it, there is no
  start step. After a batch of edits, run `catenary diagnostics` to see the
  errors and warnings for every file you touched; it then clears the set.
  Diagnostics are *pulled*: you get them only when you run the command.
  Nothing is ever pushed into another command's output.

Deny-as-guidance
  A blocked command is not a wall — the denial names the command to run
  instead (`grep` → `catenary grep`, `ls`/`find` → `catenary glob`, raw
  `sed -i` → `catenary sed`). Read the reason and run the named command.

Run catenary commands bare
  `catenary diagnostics` and `catenary sed --in-place` stand alone — no
  pipes, no `&&`/`;` chaining. Run each as its own step and read the result.
  Quote glob patterns (`catenary grep 'fn main' 'src/**/*.rs'`) so Catenary
  expands them gitignore-aware rather than the shell.

Navigate directly
  Locate files with `catenary glob`, search with `catenary grep`. Never
  brute-force by piping shell output through filters, and never pipe
  Catenary's own output through `head`/`tail`/`wc` — use `--page` and
  `--count`. Results are LSP-enriched automatically wherever a server covers
  the file. Where none does, `catenary grep` only *flags* the location: open
  the file and read it — there is no grep-fragment middle ground.";

/// Print an overview of Catenary's workflow and commands.
///
/// Two layers: a hand-written workflow preamble ([`PRIMER_PREAMBLE`], the
/// stable invariants) followed by an auto-generated reference. The reference
/// extracts the agent-facing subcommands from the derive-generated CLI
/// definition, so when help text changes (via doc comments on the derive
/// structs) or a flag is added, `primer` updates automatically.
///
/// `editing` is deliberately absent from the reference: editing starts
/// implicitly on the first edit (`catenary editing start` survives only as
/// an idempotent no-op) and `editing stop` was renamed to `diagnostics`, so
/// neither belongs in agent-facing guidance.
fn run_primer(out: &mut cli::Output) {
    use clap::CommandFactory;
    let _ = out.writeln(format_args!("{PRIMER_PREAMBLE}"));
    let app = Args::command();
    let agent_commands = ["grep", "glob", "sed", "diagnostics", "roots", "commands"];
    for name in agent_commands {
        let Some(sub) = app.find_subcommand(name) else {
            continue;
        };
        let _ = out.writeln(format_args!(
            "\n─────────────────────────────────────────────────"
        ));
        let mut sub = sub.clone();
        sub = sub
            .bin_name(format!("catenary {name}"))
            .disable_help_subcommand(true);
        let help = sub.render_help();
        let _ = out.writeln(format_args!("{help}"));
    }
}

/// Builds a standard tokio multi-thread runtime.
///
/// Used by subcommands that need async (Doctor, in-process MCP server
/// on non-Unix). The daemon builds its own runtime with larger stacks.
fn build_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")
}

/// Launch the interactive TUI dashboard.
///
/// Renders the daemon's `state.json` snapshot as a live ops board (server and
/// session health). The dashboard never reads the firehose — `catenary query`
/// is the separate firehose surface.
///
/// # Errors
///
/// Returns an error if configuration loading or TUI initialisation fails.
fn run_dashboard() -> Result<()> {
    let config = catenary_mcp::config::Config::load()?;
    catenary_mcp::tui::run(config.icons.unwrap_or_default())
}

/// Runs the Catenary daemon on a dedicated thread with a 16 MB stack.
///
/// The synchronous initialization path (`Config::load` →
/// `Session::new` → `LspClientManager::new`) has a deep call stack
/// that exceeds the default 8 MB main thread stack in debug builds.
/// Only the accept loop runs async via a tiny `block_on` future.
///
/// # Errors
///
/// Returns an error if setup or the accept loop fails.
#[cfg(unix)]
fn run_daemon() -> Result<()> {
    std::thread::Builder::new()
        .name("daemon".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(run_daemon_main)
        .context("spawn daemon thread")?
        .join()
        .map_err(|_| anyhow::anyhow!("daemon thread panicked"))?
}

/// Daemon entry point, runs on a thread with a large stack.
#[cfg(unix)]
#[allow(
    clippy::too_many_lines,
    reason = "Daemon setup requires sequential initialization steps"
)]
#[allow(
    clippy::significant_drop_tightening,
    reason = "SessionManager lifetime is correct — explicit drop(manager) at function end"
)]
fn run_daemon_main() -> Result<()> {
    use catenary_mcp::router::SessionManager;

    // Build runtime first — bind_daemon_sockets needs the tokio
    // reactor for UnixListener::bind.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build daemon runtime")?;
    let _rt_guard = rt.enter();

    // Bind sockets immediately so bridge proxies can connect while
    // heavy initialization (config, DB, LSP servers) proceeds.
    let sockets = catenary_mcp::router::bind_daemon_sockets()?;

    let logging = LoggingServer::new();
    // Floor the tracing stream before it reaches the DB sink. Without a filter
    // the registry captures everything down to TRACE, and the `log`->`tracing`
    // bridge (third-party crates) spews debug events persisted to `messages`
    // forever (no row retention) — the multi-GB DB wedge. The flood is third-party
    // `log` records (measured: ~99.8% `ignore::walk`, emitted during directory
    // scans). The bridge tags each event with its ORIGIN MODULE PATH as the
    // tracing target (`ignore::walk`, …), NOT a literal `log` target — so the old
    // `debug,log=warn` directive never matched it. Default everything to `warn` and
    // allowlist Catenary's own crates (`catenary` bin, `catenary_mcp` lib) at
    // `debug`. Override with CATENARY_LOG.
    let filter = tracing_subscriber::EnvFilter::try_from_env("CATENARY_LOG").unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("warn,catenary=debug,catenary_mcp=debug")
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(logging.clone())
        .init();

    // One-time reclaim of the legacy SQLite database (observability ticket 07).
    // Safe here: the socket bind above proved we are the sole daemon.
    drain_legacy_db();

    let config = catenary_mcp::config::Config::load()?;

    let raw_roots: Vec<PathBuf> = match std::env::var("CATENARY_ROOTS") {
        Ok(val) if !val.is_empty() => std::env::split_paths(&val).collect(),
        _ => vec![PathBuf::from(".")],
    };
    let roots: Vec<PathBuf> = raw_roots
        .into_iter()
        .map(|r| r.canonicalize())
        .collect::<std::io::Result<Vec<_>>>()?;

    let workspace_display = roots
        .iter()
        .map(|r| r.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(", ");

    let disabled = roots
        .first()
        .and_then(|r| catenary_mcp::config::load_project_config(r).ok().flatten())
        .is_some_and(|pc| !pc.lsp);

    let shared_session = if disabled {
        info!("Catenary disabled by .catenary.toml (lsp = false) in {workspace_display}");
        // Activate with just the desktop notification sink so stale hook
        // detection can still fire OS notifications.
        let desktop_enabled = config
            .notifications
            .as_ref()
            .and_then(|n| n.desktop)
            .unwrap_or(true);
        let desktop_sink =
            catenary_mcp::notify::DesktopNotificationSink::with_enabled(desktop_enabled);
        logging.activate(vec![desktop_sink]);
        None
    } else {
        let instance_id: Arc<str> = format!("daemon:{}", uuid::Uuid::new_v4()).into();

        // Firehose reaping knobs, captured before `config` moves into the
        // session (ticket 01).
        let reap_policy = config.reap_policy();
        let retention_days = config.log_retention_days;

        // Sweep runtime-dir overflow files left by crashed/ended sessions
        // (tickets 11 + 11a). At startup no session is connected yet, so every
        // diagnostics/sed overflow file belongs to a dead prior daemon and is
        // reaped. Authoritative GC — no teardown signal is reliable across hosts.
        sweep_diagnostics_overflow();
        sweep_sed_overflow();

        let threshold: catenary_mcp::logging::Severity = config
            .notifications
            .as_ref()
            .map_or_else(catenary_mcp::config::SeverityConfig::default, |n| {
                n.threshold
            })
            .into();
        let notification_router = Arc::new(
            catenary_mcp::logging::notification_router::NotificationRouter::new(threshold),
        );

        // Daemon-owned live-state snapshot. Mirrors server lifecycle/progress
        // and the alert ring to runtime_dir()/catenary/state.json — the
        // out-of-process surface that replaces the language_servers table.
        let snapshot = catenary_mcp::state_snapshot::SnapshotWriter::new(
            rt.handle(),
            &catenary_mcp::paths::runtime_dir().join("catenary"),
            catenary_mcp::state_snapshot::DaemonInfo {
                instance_id: instance_id.to_string(),
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                started_at: catenary_mcp::state_snapshot::now_iso(),
            },
        );

        let session = Arc::new(catenary_mcp::bridge::session::Session::new(
            config,
            roots,
            logging.clone(),
            instance_id.clone(),
            rt.handle().clone(),
            notification_router,
            Some(snapshot),
        ));

        // Spawn LSP servers in the background.
        let session_for_spawn = session.clone();
        rt.spawn(async move { session_for_spawn.spawn_all().await });

        // Firehose reaping (ticket 01). Run the startup instance cap once —
        // every non-self instance dir belongs to a dead daemon (one daemon per
        // host) — then schedule the periodic staleness sweep. On-write reaping
        // (rotation + per-tool byte budget) rides the JSONL sink itself.
        {
            let cache_root = catenary_mcp::paths::cache_dir().join("catenary");
            let self_inst = instance_id.to_string();
            rt.spawn_blocking(move || {
                catenary_mcp::logging::reaper::reap_instances(
                    &cache_root,
                    &self_inst,
                    reap_policy,
                    std::time::SystemTime::now(),
                );
            });

            let firehose_root = catenary_mcp::paths::cache_dir()
                .join("catenary")
                .join(instance_id.as_ref());
            let state_json = catenary_mcp::paths::runtime_dir()
                .join("catenary")
                .join("state.json");
            rt.spawn(async move {
                let mut ticker =
                    tokio::time::interval(catenary_mcp::logging::reaper::STALENESS_SWEEP_INTERVAL);
                ticker.tick().await; // consume the immediate first tick
                loop {
                    ticker.tick().await;
                    let root = firehose_root.clone();
                    let state = state_json.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        catenary_mcp::logging::reaper::sweep_stale(
                            &root,
                            &state,
                            retention_days,
                            std::time::SystemTime::now(),
                        );
                    })
                    .await;
                }
            });
        }

        Some(session)
    };

    let session_for_shutdown = shared_session.clone();

    let manager = SessionManager::from_sockets(sockets, logging);
    let manager = match shared_session {
        Some(session) => manager.with_session(session),
        None => manager,
    };

    info!(
        source = Source::DaemonLifecycle.as_str(),
        "daemon serving workspace: {workspace_display}",
    );

    // Check installed hooks against expected — fire error!() (→ desktop
    // notification) if stale. The LoggingServer is active at this point,
    // so error events route through the desktop notification sink.
    check_stale_hooks();

    // Wire signals to the daemon's shutdown token so accept_loop
    // exits on SIGINT/SIGTERM.
    let shutdown = manager.shutdown_token();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("register SIGTERM")?;
    rt.spawn(async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    "received SIGINT",
                );
            }
            _ = async { sigterm.recv().await } => {
                info!(
                    source = Source::DaemonLifecycle.as_str(),
                    "received SIGTERM",
                );
            }
        }
        shutdown.cancel();
    });

    let result = rt.block_on(manager.accept_loop());

    // Graceful LSP shutdown.
    if let Some(session) = session_for_shutdown {
        info!(
            source = Source::DaemonLifecycle.as_str(),
            "shutting down LSP servers",
        );
        rt.block_on(session.shutdown());
        // Flush the JSONL firehose (drain queue + join writer) after LSP
        // shutdown so its final telemetry is captured before exit.
        session.flush_telemetry();
    }

    // Drop removes socket files.
    drop(manager);

    info!(source = Source::DaemonLifecycle.as_str(), "daemon stopped",);

    result
}

/// Remove diagnostics overflow files left by previous daemon runs.
///
/// Runs once at daemon startup, before any session connects, so no overflow
/// file belongs to a live session — every one is from a dead prior daemon and
/// is reclaimed (the live set is empty). Authoritative GC: no teardown signal
/// is reliable across hosts (ticket 11). A graceful per-session end reclaims
/// its own file immediately (router `handle_hook_dispatch`); this sweeps
/// whatever a crash left behind. Best-effort: a filesystem error leaves files
/// in place — they are tiny and reaped on a later run.
#[cfg(unix)]
fn sweep_diagnostics_overflow() {
    let live: std::collections::HashSet<String> = std::collections::HashSet::new();
    let removed = catenary_mcp::bridge::overflow::sweep_diagnostics(
        &catenary_mcp::paths::runtime_dir(),
        &live,
    );
    if removed > 0 {
        info!("swept {removed} stale diagnostics overflow file(s)");
    }
}

/// Remove `sed-*` preview overflow files left by a previous daemon.
///
/// Each preview mints a fresh per-invocation UUID (no session to key on), so a
/// prior daemon's previews are unreferenced and reaped wholesale at startup
/// (ticket 11a). An in-lifetime last-N cap bounds the dir while the daemon runs.
#[cfg(unix)]
fn sweep_sed_overflow() {
    let removed = catenary_mcp::bridge::overflow::sweep_sed(&catenary_mcp::paths::runtime_dir());
    if removed > 0 {
        info!("swept {removed} stale sed preview overflow file(s)");
    }
}

/// One-time reclaim of the legacy `SQLite` database (observability ticket 07).
///
/// Older daemons left a `catenary.db` (plus its `-wal` / `-shm` siblings) under
/// [`state_dir`](catenary_mcp::paths::state_dir). `SQLite` is gone; the file is
/// regenerable telemetry the daemon owned, so it is deleted outright on startup
/// — no prompt, no migration. Safe here: the socket bind earlier proved this is
/// the sole daemon.
#[cfg(unix)]
fn drain_legacy_db() {
    let db = catenary_mcp::paths::state_dir()
        .join("catenary")
        .join("catenary.db");
    let reclaimed = drain_db_at(&db);
    if reclaimed > 0 {
        info!(
            source = Source::DaemonLifecycle.as_str(),
            "reclaimed legacy catenary.db ({reclaimed} bytes)",
        );
    }
}

/// Delete `db` plus its `-wal` / `-shm` siblings, returning the total bytes
/// reclaimed (0 when none exist). Best-effort: a file that cannot be removed is
/// skipped and not counted.
#[cfg(unix)]
fn drain_db_at(db: &Path) -> u64 {
    let mut reclaimed: u64 = 0;
    for suffix in ["", "-wal", "-shm"] {
        let mut os = db.to_path_buf().into_os_string();
        os.push(suffix);
        let path = PathBuf::from(os);
        if let Ok(meta) = std::fs::metadata(&path) {
            let size = meta.len();
            if std::fs::remove_file(&path).is_ok() {
                reclaimed += size;
            }
        }
    }
    reclaimed
}

/// Stops the running Catenary daemon.
///
/// Connects to the daemon's IPC socket and sends a shutdown request.
/// If no daemon is running, prints a message and returns successfully.
///
/// # Errors
///
/// Returns an error if the shutdown request fails after connecting.
#[cfg(unix)]
async fn run_stop(out: &mut cli::Output) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();

    let Ok(stream) = tokio::net::UnixStream::connect(&ipc_path).await else {
        let _ = out.writeln(format_args!("No daemon running"));
        return Ok(());
    };

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"method": "tool/shutdown"});
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let _ = out.writeln(format_args!("Daemon stopped"));
    Ok(())
}

/// Runs a grep query against the running daemon.
///
/// Connects to the daemon's IPC socket, sends a [`GrepRequest`], and
/// prints the rendered output to stdout. The daemon resolves relative
/// patterns against `cwd` and dispatches to the grep pipeline.
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
#[allow(
    clippy::too_many_arguments,
    reason = "1:1 with the clap-parsed grep flags"
)]
async fn run_grep(
    out: &mut cli::Output,
    pattern: String,
    paths: Vec<PathBuf>,
    exclude: Option<String>,
    page: usize,
    count: bool,
    include_gitignored: bool,
    include_hidden: bool,
) -> Result<()> {
    use catenary_mcp::router::{GrepRequest, METHOD_GREP};

    let cwd = std::env::current_dir().context("cannot determine working directory")?;

    let resolved = resolve_search_paths(&paths, &cwd);
    // No path arguments means a cwd-scoped search; otherwise query only when
    // at least one argument resolved to a path or pattern.
    let queried = paths.is_empty() || !resolved.forward.is_empty();
    let kind = SearchKind::Grep {
        pattern: pattern.clone(),
        bre_alternation: pattern.contains("\\|"),
    };

    let response = if queried {
        let request = GrepRequest {
            cwd: Some(cwd.clone()),
            pattern,
            paths: resolved.forward.clone(),
            exclude,
            page,
            count,
            include_gitignored,
            include_hidden,
        };
        search_ipc(METHOD_GREP, &request).await?
    } else {
        SearchResponse::default()
    };

    if count {
        render_grep_count(
            out,
            response.matches.unwrap_or(0),
            response.files.unwrap_or(0),
        );
    } else {
        render_search_outcome(out, &cwd, &resolved, &response.output, queried, &kind);
    }
    Ok(())
}

/// Parsed daemon response for `catenary grep`/`glob` over IPC.
///
/// A normal query carries rendered `output`; a `--count` query carries the
/// totals instead (`matches`/`files` for grep, `paths` for glob) with an
/// empty `output`. Fields absent from the wire default to empty/`None`, so an
/// empty response line deserializes to [`SearchResponse::default`].
#[cfg(unix)]
#[derive(Default, serde::Deserialize)]
struct SearchResponse {
    /// Rendered tree output (empty for a count response).
    #[serde(default)]
    output: String,
    /// grep `--count`: matching-line total.
    #[serde(default)]
    matches: Option<usize>,
    /// grep `--count`: distinct-file total.
    #[serde(default)]
    files: Option<usize>,
    /// glob `--count`: resolved-path total.
    #[serde(default)]
    paths: Option<usize>,
}

/// Renders the `catenary grep --count` summary: `N matches in M files`.
///
/// `matches` is the matching-line total (one per rendered leaf row, keywords
/// dropped); `files` is the number of distinct files holding them.
fn render_grep_count(out: &mut cli::Output, matches: usize, files: usize) {
    let _ = out.writeln(format_args!("{matches} matches in {files} files"));
}

/// Renders the `catenary glob --count` summary: `N paths`.
fn render_glob_count(out: &mut cli::Output, paths: usize) {
    let _ = out.writeln(format_args!("{paths} paths"));
}

/// Sends a `tool/grep` or `tool/glob` request to the daemon and returns the
/// parsed [`SearchResponse`].
///
/// Connects to the daemon IPC socket, serializes `request` with `method`
/// injected, and reads the single response line. An empty response line maps
/// to a default [`SearchResponse`] (the caller renders the empty outcome). A
/// non-zero exit is reserved for genuine faults — no daemon, transport
/// failure, or a malformed response — so soft conditions never cancel a
/// parallel tool batch.
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
async fn search_ipc<R: serde::Serialize + Sync>(
    method: &str,
    request: &R,
) -> Result<SearchResponse> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();
    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;
    let (reader, mut writer) = stream.into_split();

    let mut envelope = serde_json::to_value(request)?;
    envelope
        .as_object_mut()
        .context("request is not an object")?
        .insert(
            "method".to_string(),
            serde_json::Value::String(method.to_string()),
        );
    let mut payload = serde_json::to_string(&envelope)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(SearchResponse::default());
    }
    serde_json::from_str(trimmed).context("invalid search response from daemon")
}

/// Runs a glob query against the running daemon.
///
/// Connects to the daemon's IPC socket, sends a [`GlobRequest`], and
/// prints the rendered output to stdout. The daemon resolves relative
/// paths against `cwd` before dispatching to the glob pipeline.
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
async fn run_glob(
    out: &mut cli::Output,
    paths: Vec<PathBuf>,
    exclude: Option<String>,
    page: usize,
    count: bool,
    include_gitignored: bool,
    include_hidden: bool,
) -> Result<()> {
    use catenary_mcp::router::{GlobRequest, METHOD_GLOB};

    let cwd = std::env::current_dir().context("cannot determine working directory")?;

    let resolved = resolve_search_paths(&paths, &cwd);
    // Glob always scopes to explicit paths (clap requires at least one); query
    // only when at least one argument resolved to a path or pattern.
    let queried = !resolved.forward.is_empty();

    let response = if queried {
        let request = GlobRequest {
            cwd: Some(cwd.clone()),
            paths: resolved.forward.clone(),
            exclude,
            page,
            count,
            include_gitignored,
            include_hidden,
        };
        search_ipc(METHOD_GLOB, &request).await?
    } else {
        SearchResponse::default()
    };

    if count {
        render_glob_count(out, response.paths.unwrap_or(0));
    } else {
        render_search_outcome(
            out,
            &cwd,
            &resolved,
            &response.output,
            queried,
            &SearchKind::Glob,
        );
    }
    Ok(())
}

/// Runs a sed substitution against the running daemon.
///
/// Resolves path arguments under the same literal-first contract as
/// `catenary grep`/`glob`, but with one deliberate divergence: a path is
/// **required** (sed must never rewrite the whole tree implicitly), so an empty
/// path list is a loud error that writes nothing. Preview is the default;
/// `--in-place` writes and folds the changed files into the diagnostics batch.
///
/// # Errors
///
/// Returns an error only on genuine faults (no daemon, transport failure,
/// malformed response) — soft conditions exit 0 so a parallel tool batch is not
/// cancelled.
#[cfg(unix)]
#[allow(
    clippy::too_many_arguments,
    clippy::fn_params_excessive_bools,
    reason = "1:1 with the clap-parsed sed flags"
)]
async fn run_sed(
    out: &mut cli::Output,
    pattern: String,
    replacement: String,
    paths: Vec<PathBuf>,
    in_place: bool,
    ignore_case: bool,
    preserve_case: bool,
    first: bool,
    exclude: Option<String>,
    include_gitignored: bool,
    include_hidden: bool,
    page: usize,
) -> Result<()> {
    use catenary_mcp::router::{METHOD_SED, SedRequest, SedResponse};

    let cwd = std::env::current_dir().context("cannot determine working directory")?;

    if paths.is_empty() {
        let _ = out.writeln(format_args!(
            "{}",
            catenary_mcp::bridge::sed::REQUIRES_PATH_MSG
        ));
        return Ok(());
    }

    let resolved = resolve_search_paths(&paths, &cwd);
    // Query only when at least one argument resolved to a path or pattern; an
    // all-missing invocation just reports the missing paths.
    let queried = !resolved.forward.is_empty();

    let response = if queried {
        let request = SedRequest {
            cwd: Some(cwd.clone()),
            pattern,
            replacement,
            paths: resolved.forward.clone(),
            in_place,
            ignore_case,
            preserve_case,
            first,
            exclude,
            include_gitignored,
            include_hidden,
            page,
        };
        sed_ipc(METHOD_SED, &request).await?
    } else {
        SedResponse::default()
    };

    render_sed_outcome(out, &cwd, &response.output);
    for path in &resolved.missing {
        let _ = out.writeln(format_args!("path does not exist: {path}"));
    }
    Ok(())
}

/// Renders a `catenary sed` outcome: the cwd anchor (always, since sed writes —
/// *where* matters) followed by the daemon-rendered preview / write summary.
///
/// The daemon owns the body (file list, per-file counts, drop report, or the
/// loud-zero `no matches for:` line), so the CLI only frames it with the cwd
/// anchor; the caller appends any `path does not exist` lines.
fn render_sed_outcome(out: &mut cli::Output, cwd: &Path, daemon_output: &str) {
    let _ = out.writeln(format_args!("cwd: {}", compress_home(cwd)));
    let body = daemon_output.trim_end_matches('\n');
    if !body.is_empty() {
        let _ = out.writeln(format_args!("{body}"));
    }
}

/// Sends a `tool/sed` request to the daemon and returns the parsed
/// [`SedResponse`].
///
/// Mirrors [`search_ipc`]: connects to the daemon IPC socket, serializes
/// `request` with `method` injected, and reads the single response line (the
/// rendered output is a JSON-escaped string, so one line suffices). A non-zero
/// exit is reserved for genuine faults.
///
/// # Errors
///
/// Returns an error if no daemon is running or the query fails.
#[cfg(unix)]
async fn sed_ipc<R: serde::Serialize + Sync>(
    method: &str,
    request: &R,
) -> Result<catenary_mcp::router::SedResponse> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();
    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;
    let (reader, mut writer) = stream.into_split();

    let mut envelope = serde_json::to_value(request)?;
    envelope
        .as_object_mut()
        .context("request is not an object")?
        .insert(
            "method".to_string(),
            serde_json::Value::String(method.to_string()),
        );
    let mut payload = serde_json::to_string(&envelope)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Ok(catenary_mcp::router::SedResponse::default());
    }
    serde_json::from_str(trimmed).context("invalid sed response from daemon")
}

/// Confirms editing mode is active on the running daemon.
///
/// Connects to the daemon's IPC socket and sends a status query.
/// The actual state transition happens in the `PreToolUse` hook — this
/// command only prints confirmation for the agent's stdout.
///
/// # Errors
///
/// Returns an error if no daemon is running.
#[cfg(unix)]
async fn run_start_editing(out: &mut cli::Output) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"method": "tool/editing-start"});
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let response: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_default();
    let status = response
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("ok");

    if status == "ok" {
        let _ = out.writeln(format_args!("editing mode active"));
    } else {
        let msg = response
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unexpected response");
        anyhow::bail!("{msg}");
    }

    Ok(())
}

/// Clean/dirty outcome of `catenary diagnostics`, mapped to the process exit
/// code by the dispatcher (`0` clean / `1` dirty). A genuine fault (no daemon,
/// IPC failure, malformed response) propagates as `Err` and exits `2`.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticsExit {
    /// No diagnostic met the dirty severity threshold.
    Clean,
    /// At least one diagnostic met the dirty severity threshold.
    Dirty,
}

/// The daemon's `tool/editing-stop` response envelope (mirrors the grep/glob
/// JSON pattern): a clean/dirty `status` plus the rendered diagnostics `output`.
#[cfg(unix)]
#[derive(Default, serde::Deserialize)]
struct DiagnosticsResponse {
    /// `"clean"` or `"dirty"`. Anything else is treated as clean.
    #[serde(default)]
    status: String,
    /// Rendered diagnostics preview (may include the overflow pointer line).
    #[serde(default)]
    output: String,
}

/// Implements `catenary diagnostics`: prints diagnostics for the edited
/// files and clears the tracked set.
///
/// Connects to the daemon's IPC socket and sends `tool/editing-stop` (the
/// internal handoff method name is unchanged by the user-facing rename).
/// The `PreToolUse` hook has already prepared the handoff — this command
/// retrieves the diagnostics, prints them, and returns the clean/dirty status
/// so the caller can set the exit code.
///
/// # Errors
///
/// Returns an error (mapped to a fault exit code) if no daemon is running, the
/// IPC fails, or the response is malformed.
#[cfg(unix)]
async fn run_done_editing(out: &mut cli::Output) -> Result<DiagnosticsExit> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = catenary_mcp::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("catenary daemon not running")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"method": "tool/editing-stop"});
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    // Read the full response (a single JSON line; read to EOF defensively).
    let mut buf_reader = BufReader::new(reader);
    let mut response = String::new();
    loop {
        let mut line = String::new();
        let n = buf_reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        response.push_str(&line);
    }

    emit_diagnostics_response(out, &response)
}

/// Parse a `tool/editing-stop` response, print its diagnostics text, and map
/// `status` to the clean/dirty exit. Split from the IPC for unit testing.
///
/// # Errors
///
/// Returns an error if the response is not valid JSON — a malformed response is
/// a fault.
#[cfg(unix)]
fn emit_diagnostics_response(out: &mut cli::Output, response: &str) -> Result<DiagnosticsExit> {
    let parsed: DiagnosticsResponse = serde_json::from_str(response.trim())
        .context("invalid diagnostics response from daemon")?;

    let trimmed = parsed.output.trim();
    if !trimmed.is_empty() {
        let _ = out.writeln(format_args!("{trimmed}"));
    }

    Ok(if parsed.status == "dirty" {
        DiagnosticsExit::Dirty
    } else {
        DiagnosticsExit::Clean
    })
}

/// Sends an add-root or rm-root request to the running daemon.
///
/// Canonicalizes the path, connects to the daemon's IPC socket, and
/// prints the result. If no daemon is running, prints an error and
/// exits non-zero.
///
/// # Errors
///
/// Returns an error if the path cannot be canonicalized or the daemon
/// request fails.
#[cfg(unix)]
async fn run_root_command(out: &mut cli::Output, path: PathBuf, method: &str) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let canonical = path
        .canonicalize()
        .with_context(|| format!("cannot resolve path: {}", path.display()))?;

    let ipc_path = catenary_mcp::router::socket_path();

    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({
        "method": method,
        "path": canonical.display().to_string(),
    });
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let response: serde_json::Value = serde_json::from_str(line.trim()).unwrap_or_default();
    let status = response
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match status {
        "ok" => {
            let verb = if method.contains("roots-add") {
                "added"
            } else {
                "removed"
            };
            let _ = out.writeln(format_args!("{verb} root: {}", canonical.display()));
        }
        "not_found" => {
            let msg = response
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("root not found in hook-managed roots");
            let _ = out.writeln(format_args!("{msg}"));
        }
        _ => {
            let msg = response
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unexpected response");
            anyhow::bail!("{msg}");
        }
    }

    Ok(())
}

/// Check installed hooks against embedded expected hooks at daemon startup.
///
/// Compares each host CLI's installed hooks with the compile-time expected
/// version. Emits `error!()` for any mismatch — the `LoggingServer`
/// routes this to the notification queue and the desktop notification
/// sink automatically.
///
/// Only checks hosts that have hooks installed (missing hosts are ignored).
#[cfg(unix)]
fn check_stale_hooks() {
    /// Expected Claude Code hooks, embedded at compile time.
    const CLAUDE_HOOKS_EXPECTED: &str = include_str!("../plugins/catenary/hooks/hooks.json");
    /// Expected Gemini CLI hooks, embedded at compile time.
    const GEMINI_HOOKS_EXPECTED: &str = include_str!("../hooks/hooks.json");
    /// Expected Antigravity CLI hooks, embedded at compile time.
    const ANTIGRAVITY_HOOKS_EXPECTED: &str =
        include_str!("../plugins/catenary-antigravity/hooks.json");

    fn normalize_json(s: &str) -> String {
        serde_json::from_str::<serde_json::Value>(s)
            .ok()
            .and_then(|v| serde_json::to_string(&v).ok())
            .unwrap_or_else(|| s.trim().to_string())
    }

    fn check_host(host: &str, installed_path: &std::path::Path, expected: &str) {
        match std::fs::read_to_string(installed_path) {
            Ok(installed) if normalize_json(&installed) == normalize_json(expected) => {}
            Ok(_) => {
                tracing::error!(
                    source = Source::HookDispatch.as_str(),
                    "Stale {host} hooks detected. Run: catenary install",
                );
            }
            Err(_) => {} // Hooks file not found — host not installed, skip.
        }
    }

    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(home);

    // Claude Code: hooks live inside the plugin install path, which is
    // recorded in installed_plugins.json. Resolve the actual path.
    if let Some(hooks_path) = resolve_claude_hooks_path(&home) {
        check_host("Claude Code", &hooks_path, CLAUDE_HOOKS_EXPECTED);
    }

    // Gemini CLI: hooks at ~/.gemini/hooks/hooks.json
    let gemini_hooks = home.join(".gemini/hooks/hooks.json");
    check_host("Gemini CLI", &gemini_hooks, GEMINI_HOOKS_EXPECTED);

    // Antigravity CLI: hooks at ~/.antigravity/hooks.json
    let antigravity_hooks = home.join(".antigravity/hooks.json");
    check_host(
        "Antigravity CLI",
        &antigravity_hooks,
        ANTIGRAVITY_HOOKS_EXPECTED,
    );
}

/// Resolve the Claude Code hooks.json path from `installed_plugins.json`.
///
/// Returns `None` if Claude Code is not installed or the plugin entry
/// cannot be resolved.
#[cfg(unix)]
fn resolve_claude_hooks_path(home: &std::path::Path) -> Option<std::path::PathBuf> {
    let plugins_file = home.join(".claude/plugins/installed_plugins.json");
    let plugins_json = std::fs::read_to_string(plugins_file).ok()?;
    let plugins: serde_json::Value = serde_json::from_str(&plugins_json).ok()?;
    let entries = plugins
        .get("plugins")
        .and_then(|p| p.get("catenary@catenary"))
        .and_then(serde_json::Value::as_array)?;
    let entry = entries.first()?;
    let install_path = entry
        .get("installPath")
        .and_then(serde_json::Value::as_str)?;
    Some(std::path::PathBuf::from(install_path).join("hooks/hooks.json"))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    // ── Legacy DB drain tests (observability ticket 07) ───────────

    #[cfg(unix)]
    #[test]
    fn drain_db_at_removes_db_and_wal_shm() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("catenary.db");
        std::fs::write(&db, vec![b'x'; 100]).expect("write db");
        std::fs::write(dir.path().join("catenary.db-wal"), vec![b'y'; 50]).expect("write wal");
        std::fs::write(dir.path().join("catenary.db-shm"), vec![b'z'; 25]).expect("write shm");

        let reclaimed = drain_db_at(&db);

        assert_eq!(reclaimed, 175, "reclaimed bytes = db + wal + shm");
        assert!(!db.exists(), "db removed");
        assert!(!dir.path().join("catenary.db-wal").exists(), "wal removed");
        assert!(!dir.path().join("catenary.db-shm").exists(), "shm removed");
    }

    #[cfg(unix)]
    #[test]
    fn drain_db_at_is_noop_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(drain_db_at(&dir.path().join("catenary.db")), 0);
    }

    // ── CLI hook subcommand tests ─────────────────────────────────

    #[test]
    fn test_cli_hook_pre_agent() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "pre-agent", "--format=claude"]);
        let args = args.expect("hook pre-agent should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::PreAgent { .. }));
    }

    #[test]
    fn test_cli_hook_pre_tool() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "pre-tool", "--format=gemini"]);
        let args = args.expect("hook pre-tool should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::PreTool { .. }));
    }

    #[test]
    fn test_cli_hook_post_agent() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "post-agent", "--format=claude"]);
        let args = args.expect("hook post-agent should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::PostAgent { .. }));
    }

    #[test]
    fn test_cli_hook_session_start() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "session-start", "--format=gemini"]);
        let args = args.expect("hook session-start should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::SessionStart { .. }));
    }

    #[test]
    fn test_cli_hook_session_end() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "session-end", "--format=claude"]);
        let args = args.expect("hook session-end should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::SessionEnd { .. }));
    }

    #[test]
    fn test_cli_hook_subagent_start() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "subagent-start", "--format=claude"]);
        let args = args.expect("hook subagent-start should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::SubagentStart { .. }));
    }

    #[test]
    fn test_cli_hook_worktree_remove() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "worktree-remove", "--format=claude"]);
        let args = args.expect("hook worktree-remove should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::WorktreeRemove { .. }));
    }

    #[test]
    fn test_cli_hook_antigravity_format() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "pre-tool", "--format=antigravity"]);
        let args = args.expect("hook pre-tool with antigravity format should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(
            command,
            HookCommand::PreTool {
                format: HostFormat::Antigravity
            }
        ));
    }

    #[test]
    fn test_cli_config_subcommand() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "config"]);
        let args = args.expect("config subcommand should parse");
        assert!(matches!(args.command, Some(Command::Config)));
    }

    // ── CLI editing subcommand tests ───────────────────────────────

    #[test]
    fn test_cli_editing_start() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "editing", "start"]);
        let args = args.expect("editing start should parse");
        assert!(matches!(
            args.command,
            Some(Command::Editing {
                command: EditingCommand::Start
            })
        ));
    }

    #[test]
    fn test_cli_diagnostics() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "diagnostics"]);
        let args = args.expect("diagnostics should parse");
        assert!(matches!(args.command, Some(Command::Diagnostics)));
    }

    #[test]
    fn editing_stop_retired() {
        // `catenary editing stop` was renamed to `catenary diagnostics`; the
        // old subcommand no longer parses.
        use clap::Parser;
        assert!(Args::try_parse_from(["catenary", "editing", "stop"]).is_err());
    }

    // ── CLI grep subcommand tests ──────────────────────────────────

    #[test]
    fn test_cli_grep_minimal() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "grep", "foo"]);
        let args = args.expect("grep with pattern should parse");
        let Some(Command::Grep {
            pattern,
            scope,
            exclude,
            page,
            count,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "foo");
        assert!(scope.is_empty());
        assert!(exclude.is_none());
        assert_eq!(page, 1);
        assert!(!count);
        assert!(!include_gitignored);
        assert!(!include_hidden);
    }

    #[test]
    fn test_cli_grep_single_path() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "grep", "foo|bar", "src/main.rs"]);
        let args = args.expect("grep with single path should parse");
        let Some(Command::Grep { pattern, scope, .. }) = args.command else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "foo|bar");
        assert_eq!(scope, vec!["src/main.rs"]);
    }

    #[test]
    fn test_cli_grep_variadic_paths() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "grep",
            "pattern",
            "src/tui/stream.rs",
            "src/tui/mod.rs",
        ]);
        let args = args.expect("grep with multiple paths should parse");
        let Some(Command::Grep { pattern, scope, .. }) = args.command else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "pattern");
        assert_eq!(scope, vec!["src/tui/stream.rs", "src/tui/mod.rs"]);
    }

    #[test]
    fn test_cli_grep_all_flags() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "grep",
            "foo|bar",
            "src/",
            "--exclude-pattern",
            "tests/",
            "--page",
            "3",
            "--count",
            "--include-gitignored",
            "--include-hidden",
        ]);
        let args = args.expect("grep with all flags should parse");
        let Some(Command::Grep {
            pattern,
            scope,
            exclude,
            page,
            count,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Grep command");
        };
        assert_eq!(pattern, "foo|bar");
        assert_eq!(scope, vec!["src/"]);
        assert_eq!(exclude.as_deref(), Some("tests/"));
        assert_eq!(page, 3);
        assert!(count);
        assert!(include_gitignored);
        assert!(include_hidden);
    }

    #[test]
    fn test_cli_grep_missing_pattern() {
        use clap::Parser;
        let result = Args::try_parse_from(["catenary", "grep"]);
        assert!(result.is_err(), "grep without pattern should fail");
    }

    // ── CLI glob subcommand tests ──────────────────────────────────

    #[test]
    fn test_cli_glob_minimal() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "glob", "src/"]);
        let args = args.expect("glob with path should parse");
        let Some(Command::Glob {
            paths,
            exclude,
            page,
            count,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Glob command");
        };
        assert_eq!(paths, vec!["src/"]);
        assert!(exclude.is_none());
        assert_eq!(page, 1);
        assert!(!count);
        assert!(!include_gitignored);
        assert!(!include_hidden);
    }

    #[test]
    fn test_cli_glob_variadic() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "glob",
            "src/tui/stream.rs",
            "src/tui/mod.rs",
            "src/tui/render.rs",
        ]);
        let args = args.expect("glob with multiple paths should parse");
        let Some(Command::Glob { paths, .. }) = args.command else {
            unreachable!("expected Glob command");
        };
        assert_eq!(
            paths,
            vec!["src/tui/stream.rs", "src/tui/mod.rs", "src/tui/render.rs"]
        );
    }

    #[test]
    fn test_cli_glob_all_flags() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "glob",
            "src/",
            "--exclude-pattern",
            "target/**",
            "--page",
            "2",
            "--count",
            "--include-gitignored",
            "--include-hidden",
        ]);
        let args = args.expect("glob with all flags should parse");
        let Some(Command::Glob {
            paths,
            exclude,
            page,
            count,
            include_gitignored,
            include_hidden,
        }) = args.command
        else {
            unreachable!("expected Glob command");
        };
        assert_eq!(paths, vec!["src/"]);
        assert_eq!(exclude.as_deref(), Some("target/**"));
        assert_eq!(page, 2);
        assert!(count);
        assert!(include_gitignored);
        assert!(include_hidden);
    }

    #[test]
    fn test_cli_glob_missing_path() {
        use clap::Parser;
        let result = Args::try_parse_from(["catenary", "glob"]);
        assert!(result.is_err(), "glob without path should fail");
    }

    // ── CLI roots subcommand tests ──────────────────────────────────

    #[test]
    fn test_cli_roots_add() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "roots", "add", "/tmp/project"]);
        let args = args.expect("roots add should parse");
        let Some(Command::Roots {
            command: RootsCommand::Add { path },
        }) = args.command
        else {
            unreachable!("expected Roots Add command");
        };
        assert_eq!(path, PathBuf::from("/tmp/project"));
    }

    #[test]
    fn test_cli_roots_rm() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "roots", "rm", "/tmp/project"]);
        let args = args.expect("roots rm should parse");
        let Some(Command::Roots {
            command: RootsCommand::Rm { path },
        }) = args.command
        else {
            unreachable!("expected Roots Rm command");
        };
        assert_eq!(path, PathBuf::from("/tmp/project"));
    }

    #[test]
    fn test_cli_roots_ls() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "roots", "ls"]);
        let args = args.expect("roots ls should parse");
        assert!(matches!(
            args.command,
            Some(Command::Roots {
                command: RootsCommand::Ls
            })
        ));
    }

    #[test]
    fn test_cli_primer() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "primer"]);
        let args = args.expect("primer should parse");
        assert!(matches!(args.command, Some(Command::Primer)));
    }

    #[test]
    fn primer_renders_preamble_and_reference() {
        // The primer is two layers: a hand-written workflow preamble and the
        // auto-generated reference for each agent-facing subcommand (bin name
        // rewritten to `catenary <name>`). Capturing through `Output::buffer`
        // proves the handler writes via `Output` (not raw `println!`) and
        // emits both layers with the final command surface.
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out);
        let text = out.into_string();

        // Preamble (the stable invariants).
        assert!(
            text.contains("The edit→diagnostics loop"),
            "primer should emit the workflow preamble"
        );
        assert!(
            text.contains("Deny-as-guidance"),
            "primer preamble should teach deny-as-guidance"
        );

        // Auto-generated reference, final surface.
        for needle in [
            "catenary grep",
            "catenary glob",
            "catenary sed",
            "catenary diagnostics",
            "catenary roots",
            "catenary commands",
            "--count",
            "--page",
        ] {
            assert!(
                text.contains(needle),
                "primer reference should document {needle}"
            );
        }
    }

    #[test]
    fn primer_no_retired_commands() {
        // `editing start` is implicit and `editing stop` was renamed to
        // `diagnostics`; neither belongs in agent-facing guidance, so the
        // reference must not advertise the `editing` subcommand at all.
        let mut out = cli::Output::buffer(80);
        run_primer(&mut out);
        let text = out.into_string();
        for retired in ["catenary editing", "editing start", "editing stop"] {
            assert!(
                !text.contains(retired),
                "primer must not mention retired `{retired}`"
            );
        }
    }

    // ── CLI install subcommand tests ────────────────────────────────

    #[test]
    fn test_cli_install_bare() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install"]);
        let args = args.expect("install should parse");
        let Some(Command::Install { host, dry_run }) = args.command else {
            unreachable!("expected Install command");
        };
        assert!(host.is_none());
        assert!(!dry_run);
    }

    #[test]
    fn test_cli_install_claude() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "claude"]);
        let args = args.expect("install claude should parse");
        let Some(Command::Install {
            host: Some(InstallHost::Claude { source }),
            ..
        }) = args.command
        else {
            unreachable!("expected Install Claude command");
        };
        assert!(source.is_none());
    }

    #[test]
    fn test_cli_install_claude_with_source() {
        use clap::Parser;
        let args = Args::try_parse_from([
            "catenary",
            "install",
            "claude",
            "/home/user/Projects/Catenary",
        ]);
        let args = args.expect("install claude with source should parse");
        let Some(Command::Install {
            host: Some(InstallHost::Claude { source }),
            ..
        }) = args.command
        else {
            unreachable!("expected Install Claude command");
        };
        assert_eq!(source.as_deref(), Some("/home/user/Projects/Catenary"));
    }

    #[test]
    fn test_cli_install_gemini() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "gemini"]);
        let args = args.expect("install gemini should parse");
        assert!(matches!(
            args.command,
            Some(Command::Install {
                host: Some(InstallHost::Gemini { .. }),
                ..
            })
        ));
    }

    #[test]
    fn test_cli_install_antigravity() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "antigravity"]);
        let args = args.expect("install antigravity should parse");
        assert!(matches!(
            args.command,
            Some(Command::Install {
                host: Some(InstallHost::Antigravity { .. }),
                ..
            })
        ));
    }

    #[test]
    fn test_cli_install_opencode() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "opencode"]);
        let args = args.expect("install opencode should parse");
        let Some(Command::Install {
            host: Some(InstallHost::OpenCode { source, workspace }),
            ..
        }) = args.command
        else {
            unreachable!("expected Install OpenCode command");
        };
        assert!(source.is_none());
        assert!(!workspace, "workspace should default to false");
    }

    #[test]
    fn test_cli_install_opencode_workspace() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "opencode", "--workspace"]);
        let args = args.expect("install opencode --workspace should parse");
        let Some(Command::Install {
            host: Some(InstallHost::OpenCode { workspace, .. }),
            ..
        }) = args.command
        else {
            unreachable!("expected Install OpenCode command");
        };
        assert!(workspace, "--workspace should set the flag");
    }

    #[test]
    fn test_cli_install_dry_run() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "install", "--dry-run", "claude"]);
        let args = args.expect("install --dry-run claude should parse");
        let Some(Command::Install { dry_run, .. }) = args.command else {
            unreachable!("expected Install command");
        };
        assert!(dry_run);
    }

    // ── CLI update subcommand tests ───────────────────────────────

    #[test]
    fn test_cli_update_bare() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "update"]);
        let args = args.expect("update should parse");
        let Some(Command::Update { check, force }) = args.command else {
            unreachable!("expected Update command");
        };
        assert!(!check);
        assert!(!force);
    }

    #[test]
    fn test_cli_update_check() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "update", "--check"]);
        let args = args.expect("update --check should parse");
        let Some(Command::Update { check, force }) = args.command else {
            unreachable!("expected Update command");
        };
        assert!(check);
        assert!(!force);
    }

    #[test]
    fn test_cli_update_force() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "update", "--force"]);
        let args = args.expect("update --force should parse");
        let Some(Command::Update { check, force }) = args.command else {
            unreachable!("expected Update command");
        };
        assert!(!check);
        assert!(force);
    }

    #[test]
    fn test_cli_update_check_and_force() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "update", "--check", "--force"]);
        let args = args.expect("update --check --force should parse");
        let Some(Command::Update { check, force }) = args.command else {
            unreachable!("expected Update command");
        };
        assert!(check);
        assert!(force);
    }

    // ── to_literal_paths tests ─────────────────────────────────────────

    #[test]
    fn test_to_literal_paths_empty() {
        let paths = to_literal_paths(vec![]);
        assert!(paths.is_empty());
    }

    #[test]
    fn test_to_literal_paths_single() {
        let paths = to_literal_paths(vec!["src/main.rs".to_string()]);
        assert_eq!(paths, vec![PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn test_to_literal_paths_multiple() {
        let paths = to_literal_paths(vec![
            "src/tui/stream.rs".to_string(),
            "src/tui/mod.rs".to_string(),
        ]);
        assert_eq!(
            paths,
            vec![
                PathBuf::from("src/tui/stream.rs"),
                PathBuf::from("src/tui/mod.rs"),
            ]
        );
    }

    // ── contains_glob_metachar tests ─────────────────────────────────

    #[test]
    fn test_contains_glob_metachar_star() {
        assert!(contains_glob_metachar("src/**/*.rs"));
    }

    #[test]
    fn test_contains_glob_metachar_question() {
        assert!(contains_glob_metachar("src/?.rs"));
    }

    #[test]
    fn test_contains_glob_metachar_bracket() {
        assert!(contains_glob_metachar("src/[ab].rs"));
    }

    #[test]
    fn test_contains_glob_metachar_brace() {
        assert!(contains_glob_metachar("src/{a,b}.rs"));
    }

    #[test]
    fn test_contains_glob_metachar_none() {
        assert!(!contains_glob_metachar("src/main.rs"));
    }

    #[test]
    fn test_contains_glob_metachar_directory() {
        assert!(!contains_glob_metachar("src/tui/"));
    }

    // ── resolve_search_paths tests ───────────────────────────────────

    #[test]
    fn resolve_existing_path_is_forwarded() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("real.rs"), "x").expect("write");
        let res = resolve_search_paths(&[PathBuf::from("real.rs")], tmp.path());
        assert_eq!(res.forward, vec![PathBuf::from("real.rs")]);
        assert!(res.missing.is_empty());
    }

    #[test]
    fn resolve_metachar_named_file_is_literal_not_pattern() {
        // A file literally named with a glob metacharacter resolves as a
        // path — existence is probed before pattern classification.
        let tmp = tempfile::tempdir().expect("tempdir");
        let name = "weird[1].rs";
        std::fs::write(tmp.path().join(name), "x").expect("write");
        let res = resolve_search_paths(&[PathBuf::from(name)], tmp.path());
        assert_eq!(res.forward, vec![PathBuf::from(name)]);
        assert!(res.missing.is_empty());
    }

    #[test]
    fn resolve_missing_glob_is_forwarded_as_pattern() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let res = resolve_search_paths(&[PathBuf::from("src/**/none.rs")], tmp.path());
        // Missing + glob metacharacter → forwarded for the daemon to expand.
        assert_eq!(res.forward, vec![PathBuf::from("src/**/none.rs")]);
        assert!(res.missing.is_empty());
    }

    #[test]
    fn resolve_missing_plain_path_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let res = resolve_search_paths(&[PathBuf::from("src/not/real.rs")], tmp.path());
        assert!(res.forward.is_empty());
        assert_eq!(res.missing, vec!["src/not/real.rs".to_string()]);
    }

    #[test]
    fn resolve_mixed_arguments_classified_independently() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("real.rs"), "x").expect("write");
        let args = [
            PathBuf::from("real.rs"),
            PathBuf::from("*.toml"),
            PathBuf::from("gone.rs"),
        ];
        let res = resolve_search_paths(&args, tmp.path());
        assert_eq!(
            res.forward,
            vec![PathBuf::from("real.rs"), PathBuf::from("*.toml")]
        );
        assert_eq!(res.missing, vec!["gone.rs".to_string()]);
    }

    // ── render_search_outcome tests ──────────────────────────────────

    #[allow(
        clippy::needless_pass_by_value,
        reason = "test helper builds owned args inline at the call site"
    )]
    fn render(paths: SearchPaths, output: &str, queried: bool, kind: SearchKind) -> String {
        let mut out = cli::Output::buffer(80);
        render_search_outcome(
            &mut out,
            Path::new("/tmp/work"),
            &paths,
            output,
            queried,
            &kind,
        );
        out.into_string()
    }

    #[test]
    fn render_results_pass_through_verbatim() {
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src")],
            missing: vec![],
        };
        let text = render(
            paths,
            "cwd: ~/work\nsrc/\n\tmain.rs",
            true,
            SearchKind::Glob,
        );
        assert!(text.contains("main.rs"), "{text}");
        assert!(!text.contains("no files matched"), "{text}");
    }

    #[test]
    fn render_glob_zero_match_is_loud() {
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src/**/none.rs")],
            missing: vec![],
        };
        let text = render(paths, "", true, SearchKind::Glob);
        assert!(text.contains("cwd:"), "cwd anchor always printed: {text}");
        assert!(text.contains("no files matched: src/**/none.rs"), "{text}");
    }

    #[test]
    fn render_missing_plain_path_is_loud() {
        let paths = SearchPaths {
            forward: vec![],
            missing: vec!["src/not/real.rs".to_string()],
        };
        let text = render(paths, "", false, SearchKind::Glob);
        assert!(text.contains("cwd:"), "{text}");
        assert!(
            text.contains("path does not exist: src/not/real.rs"),
            "{text}"
        );
    }

    #[test]
    fn render_cwd_printed_on_empty() {
        let paths = SearchPaths {
            forward: vec![],
            missing: vec![],
        };
        let text = render(paths, "", true, SearchKind::Glob);
        assert!(
            text.contains("cwd:"),
            "empty result still anchors cwd: {text}"
        );
    }

    #[test]
    fn render_grep_empty_echoes_pattern_and_scope() {
        let paths = SearchPaths {
            forward: vec![PathBuf::from("src")],
            missing: vec![],
        };
        let text = render(
            paths,
            "",
            true,
            SearchKind::Grep {
                pattern: "needle".to_string(),
                bre_alternation: false,
            },
        );
        assert!(text.contains("no matches for: needle"), "{text}");
        assert!(text.contains("searched: src"), "{text}");
    }

    #[test]
    fn render_grep_bre_alternation_hint() {
        let paths = SearchPaths {
            forward: vec![],
            missing: vec![],
        };
        let text = render(
            paths,
            "",
            true,
            SearchKind::Grep {
                pattern: "foo\\|bar".to_string(),
                bre_alternation: true,
            },
        );
        assert!(text.contains("alternation"), "BRE hint shown: {text}");
    }

    #[test]
    fn render_not_queried_skips_zero_result_line() {
        // All arguments missing: no query ran, so no "no matches for" —
        // the path-does-not-exist lines carry the explanation.
        let paths = SearchPaths {
            forward: vec![],
            missing: vec!["gone.rs".to_string()],
        };
        let text = render(
            paths,
            "",
            false,
            SearchKind::Grep {
                pattern: "needle".to_string(),
                bre_alternation: false,
            },
        );
        assert!(!text.contains("no matches for"), "{text}");
        assert!(text.contains("path does not exist: gone.rs"), "{text}");
    }

    // ── --count rendering tests ────────────────────────────────────

    #[test]
    fn grep_count_matches_in_files() {
        let mut out = cli::Output::buffer(80);
        render_grep_count(&mut out, 12, 3);
        assert_eq!(out.into_string(), "12 matches in 3 files\n");
    }

    #[test]
    fn grep_count_zero_is_well_formed() {
        let mut out = cli::Output::buffer(80);
        render_grep_count(&mut out, 0, 0);
        assert_eq!(out.into_string(), "0 matches in 0 files\n");
    }

    #[test]
    fn glob_count_paths() {
        let mut out = cli::Output::buffer(80);
        render_glob_count(&mut out, 7);
        assert_eq!(out.into_string(), "7 paths\n");
    }

    // ── diagnostics exit-code contract (ticket 11) ─────────────────

    #[cfg(unix)]
    #[test]
    fn diagnostics_clean_exit_0() {
        let mut out = cli::Output::buffer(80);
        let status =
            emit_diagnostics_response(&mut out, r#"{"status":"clean","output":"[clean]"}"#)
                .expect("clean response parses");
        assert_eq!(status, DiagnosticsExit::Clean);
        assert!(out.into_string().contains("[clean]"));
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_dirty_exit_1() {
        let mut out = cli::Output::buffer(80);
        let status = emit_diagnostics_response(
            &mut out,
            r#"{"status":"dirty","output":":1:1 [error] e: boom"}"#,
        )
        .expect("dirty response parses");
        assert_eq!(status, DiagnosticsExit::Dirty);
        assert!(out.into_string().contains("boom"));
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_warnings_only_exit_0() {
        // A warnings-only run is reported clean by the daemon → exit 0.
        let mut out = cli::Output::buffer(80);
        let status = emit_diagnostics_response(
            &mut out,
            r#"{"status":"clean","output":":2:1 [warning] w: meh"}"#,
        )
        .expect("response parses");
        assert_eq!(status, DiagnosticsExit::Clean);
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_malformed_response_is_fault() {
        // A malformed response is a fault (mapped to exit 2 by the dispatcher),
        // not silently treated as clean.
        let mut out = cli::Output::buffer(80);
        assert!(emit_diagnostics_response(&mut out, "not json").is_err());
    }
}
