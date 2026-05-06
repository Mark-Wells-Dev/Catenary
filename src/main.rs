// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary MCP server and CLI.
//!
//! This is the main entry point for the Catenary multiplexing bridge.
//! It can be run as an MCP server or as a CLI tool to list and monitor sessions.

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use catenary_mcp::bridge::McpRouter;
use catenary_mcp::cli::{self, HostFormat, QueryFormat};
use catenary_mcp::logging::LoggingServer;
use catenary_mcp::mcp::McpServer;
use catenary_mcp::session::{self, Session};
use catenary_mcp::source::Source;

/// Command-line arguments for Catenary.
#[derive(Parser, Debug)]
#[command(name = "catenary")]
#[command(about = "Multiplexing bridge between MCP and multiple LSP servers")]
#[command(version = env!("CATENARY_VERSION"))]
struct Args {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Subcommands supported by Catenary.
#[derive(Subcommand, Debug)]
enum Command {
    /// List active Catenary sessions.
    List,

    /// Monitor events from a session.
    Monitor {
        /// Session ID or row number (use 'catenary list' to see available sessions).
        id: String,

        /// Show raw JSON output.
        #[arg(long)]
        raw: bool,

        /// Disable colored output.
        #[arg(long)]
        nocolor: bool,

        /// Filter events by regex pattern.
        #[arg(long, short)]
        filter: Option<String>,
    },

    /// Show status of a session.
    Status {
        /// Session ID (use 'catenary list' to see available sessions).
        id: String,
    },

    /// Output a recommended annotated config template.
    Config,

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

    /// Hook subcommands (invoked by host CLI hooks).
    Hook {
        #[command(subcommand)]
        command: HookCommand,
    },

    /// Query events from the database.
    Query {
        /// Filter by session ID or prefix.
        #[arg(long)]
        session: Option<String>,

        /// Time filter (e.g., "1h", "today", "7d", "30m").
        #[arg(long)]
        since: Option<String>,

        /// Filter by event kind (e.g., `tool_call`, `diagnostics`).
        #[arg(long)]
        kind: Option<String>,

        /// Free-text search in event payload.
        #[arg(long)]
        search: Option<String>,

        /// Raw SQL query (power users).
        #[arg(long)]
        sql: Option<String>,

        /// Output format.
        #[arg(long, value_enum, default_value = "table")]
        format: QueryFormat,
    },

    /// Garbage-collect old session data.
    Gc {
        /// Delete events older than this duration (e.g., "7d", "30d").
        #[arg(long)]
        older_than: Option<String>,

        /// Delete all data for dead sessions.
        #[arg(long)]
        dead: bool,

        /// Delete all data for a specific session.
        #[arg(long)]
        session: Option<String>,
    },
}

/// Hook subcommands invoked by host CLI hooks.
#[derive(Subcommand, Debug)]
enum HookCommand {
    /// Pre-agent: signal turn start (`UserPromptSubmit` / `BeforeAgent`).
    #[command(name = "pre-agent")]
    PreAgent {
        /// Output format: "claude" or "gemini".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// Pre-tool: editing state enforcement (`PreToolUse` / `BeforeTool`).
    #[command(name = "pre-tool")]
    PreTool {
        /// Output format: "claude" or "gemini".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// Post-tool: file-change notification with diagnostics (`PostToolUse` / `AfterTool`).
    #[command(name = "post-tool")]
    PostTool {
        /// Output format: "claude" or "gemini".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// Post-agent: force `done_editing` before agent finishes (`Stop` / `AfterAgent`).
    #[command(name = "post-agent")]
    PostAgent {
        /// Output format: "claude" or "gemini".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
    /// `SessionStart`: clear stale editing state.
    #[command(name = "session-start")]
    SessionStart {
        /// Output format: "claude" or "gemini".
        #[arg(long, value_enum)]
        format: HostFormat,
    },
}

/// Entry point for the Catenary binary.
///
/// # Errors
///
/// Returns an error if the subcommand fails.
#[tokio::main]
#[allow(clippy::too_many_lines, reason = "Dispatch table for all subcommands")]
async fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        None => {
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                run_dashboard()
            } else {
                run_server().await
            }
        }
        Some(Command::List) => cli::commands::run_list(),
        Some(Command::Config) => {
            cli::config_template::print_template();
            Ok(())
        }
        Some(Command::Monitor {
            id,
            raw,
            nocolor,
            filter,
        }) => cli::commands::run_monitor(&id, raw, nocolor, filter.as_deref()),
        Some(Command::Status { id }) => cli::commands::run_status(&id),
        Some(Command::Doctor {
            server,
            root,
            nocolor,
            diff,
        }) => {
            if let Some(server_name) = server {
                cli::doctor::run_doctor_single(&server_name, &root, nocolor).await
            } else {
                cli::doctor::run_doctor(&root, nocolor, diff).await
            }
        }
        Some(Command::Hook { command }) => {
            match command {
                HookCommand::PreAgent { format } => cli::hooks::run_pre_agent(format),
                HookCommand::PreTool { format } => cli::hooks::run_pre_tool(format),
                HookCommand::PostTool { format } => cli::hooks::run_post_tool(format),
                HookCommand::PostAgent { format } => cli::hooks::run_post_agent(format),
                HookCommand::SessionStart { format } => cli::hooks::run_session_start(format),
            }
            Ok(())
        }
        Some(Command::Query {
            session,
            since,
            kind,
            search,
            sql,
            format,
        }) => {
            let conn = catenary_mcp::db::open_and_migrate()?;
            cli::commands::run_query(
                &conn,
                session.as_deref(),
                since.as_deref(),
                kind.as_deref(),
                search.as_deref(),
                sql.as_deref(),
                format,
            )
        }
        Some(Command::Gc {
            older_than,
            dead,
            session,
        }) => {
            let conn = catenary_mcp::db::open_and_migrate()?;
            cli::commands::run_gc(&conn, older_than.as_deref(), dead, session.as_deref())
        }
    }
}

/// Launch the interactive TUI dashboard.
///
/// Prunes stale sessions based on the configured retention policy, then
/// enters a two-pane terminal interface showing sessions and events.
///
/// # Errors
///
/// Returns an error if configuration loading, session pruning, or TUI
/// initialisation fails.
fn run_dashboard() -> Result<()> {
    let config = catenary_mcp::config::Config::load()?;

    let conn = catenary_mcp::db::open_and_migrate()?;
    if let Err(e) = session::prune_sessions_with_conn(&conn, config.log_retention_days) {
        info!("session pruning failed: {e}");
    }

    catenary_mcp::tui::run(config.icons.unwrap_or_default())
}

/// Runs the MCP server.
///
/// # Errors
///
/// Returns an error if the server fails to start or encounters an internal error.
#[allow(
    clippy::too_many_lines,
    reason = "Server setup requires sequential initialization steps"
)]
async fn run_server() -> Result<()> {
    let logging = LoggingServer::new();

    tracing_subscriber::registry().with(logging.clone()).init();

    // Load configuration
    let config = catenary_mcp::config::Config::load()?;

    // Bootstrap roots from CATENARY_ROOTS env var (path-separated) or default to cwd.
    // MCP client overrides via initialize params.
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

    // Check primary root for `lsp = false` in `.catenary.toml`.
    let disabled = roots
        .first()
        .and_then(|r| catenary_mcp::config::load_project_config(r).ok().flatten())
        .is_some_and(|pc| !pc.lsp);

    if disabled {
        info!(
            "Catenary disabled by .catenary.toml (lsp = false) in {}",
            workspace_display
        );
        return run_disabled_server(logging).await;
    }

    // Create session for observability
    let session = Arc::new(std::sync::Mutex::new(Session::create(&workspace_display)?));
    info!("Starting catenary multiplexing bridge");
    info!(
        "Session ID: {}",
        session
            .lock()
            .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
            .info
            .id
    );
    info!("Workspace roots: {}", workspace_display);
    let instance_id: Arc<str> = session
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
        .info
        .id
        .as_str()
        .into();

    let session_conn = session
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
        .conn()
        .clone();

    let toolbox = Arc::new(catenary_mcp::bridge::session::Session::new(
        config.clone(),
        roots,
        logging,
        session_conn,
        instance_id.clone(),
        tokio::runtime::Handle::current(),
    ));
    toolbox.spawn_all().await;

    // Start the hook server for PostToolUse/PreToolUse hook integration
    let hook_conn = session
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
        .conn()
        .clone();
    let hook_server = catenary_mcp::hook::HookServer::new(
        toolbox.clone(),
        hook_conn,
        instance_id,
        "host".to_string(),
    );
    let socket_path = session
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
        .socket_path();
    let notify_handle = hook_server.start(&socket_path)?;
    session
        .lock()
        .map_err(|_| anyhow::anyhow!("mutex poisoned"))?
        .set_socket_active();

    let roots_refresh = toolbox.roots_refresh_requested.clone();
    let toolbox_for_roots = toolbox.clone();
    let toolbox_for_shutdown = toolbox.clone();
    let handler = McpRouter::new(toolbox);

    // Run MCP server (blocking - reads from stdin)
    let session_for_callback = session.clone();
    let runtime_for_roots = tokio::runtime::Handle::current();
    let mut mcp_server = McpServer::new(handler, toolbox_for_roots.logging.clone())
        .on_roots_refresh(roots_refresh)
        .on_client_info(Box::new(move |name: &str, version: &str| {
            if let Ok(mut session) = session_for_callback.lock() {
                session.set_client_info(name, version);
            }
        }))
        .on_roots_changed(Box::new(move |roots| {
            let mut paths: Vec<PathBuf> = roots
                .iter()
                .filter_map(|root| {
                    root.uri.strip_prefix("file://").and_then(|p| {
                        let path = PathBuf::from(p);
                        match path.canonicalize() {
                            Ok(canonical) => Some(canonical),
                            Err(e) => {
                                warn!(
                                    source = Source::ConfigValidation.as_str(),
                                    "Skipping root {p}: {e}",
                                );
                                None
                            }
                        }
                    })
                })
                .collect();

            // Merge stored transcript roots so fetch_roots can't overwrite
            // them with a roots/list response that omits /add-dir roots.
            toolbox_for_roots.merge_transcript_roots(&mut paths);

            runtime_for_roots.block_on(toolbox_for_roots.sync_roots(paths))?;
            Ok(())
        }));

    // Run in a blocking task since MCP server uses synchronous I/O
    let mcp_task = tokio::task::spawn_blocking(move || mcp_server.run());

    // Wait for either the MCP task to finish or a termination signal.
    // On Unix, also catch SIGTERM so the host CLI killing us triggers
    // graceful LSP shutdown instead of orphaning child processes.
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let mcp_result = tokio::select! {
        res = mcp_task => {
            res?
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Received shutdown signal");
            Ok(())
        }
        _ = async {
            #[cfg(unix)]
            { sigterm.recv().await }
            #[cfg(not(unix))]
            { std::future::pending::<Option<()>>().await }
        } => {
            info!("Received SIGTERM");
            Ok(())
        }
    };

    // Stop notify socket server
    notify_handle.abort();
    let _ = notify_handle.await;

    // Shutdown LSP clients gracefully
    info!("Shutting down LSP servers");
    toolbox_for_shutdown.shutdown().await;

    // Mark session dead explicitly — Drop may not run because
    // spawn_blocking holds an Arc<Session> clone that outlives the runtime.
    if let Ok(s) = session.lock() {
        s.mark_dead();
    }

    mcp_result
}

/// Runs a minimal MCP server for disabled sessions.
///
/// When `.catenary.toml` has `enabled = false`, Catenary runs as a thin
/// protocol stub: responds to MCP `initialize` and `tools/list` (empty),
/// but creates no session, spawns no LSP servers, starts no hook IPC
/// server, and writes nothing to the database. Hook CLI processes fail
/// to connect and output nothing, so the host treats all hooks as "allow."
async fn run_disabled_server(logging: LoggingServer) -> Result<()> {
    /// Tool handler that exposes no tools.
    struct DisabledHandler;

    impl catenary_mcp::mcp::ToolHandler for DisabledHandler {
        fn list_tools(&self) -> Vec<catenary_mcp::mcp::Tool> {
            Vec::new()
        }

        fn call_tool(
            &self,
            _name: &str,
            _arguments: Option<serde_json::Value>,
            _parent_id: Option<i64>,
            _cancel: &tokio_util::sync::CancellationToken,
        ) -> Result<catenary_mcp::mcp::CallToolResult> {
            Err(anyhow::anyhow!("Catenary is disabled for this workspace"))
        }
    }

    let mut server = McpServer::new(DisabledHandler, logging);
    let mcp_task = tokio::task::spawn_blocking(move || server.run());
    mcp_task.await?
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

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
    fn test_cli_hook_post_tool() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "hook", "post-tool", "--format=claude"]);
        let args = args.expect("hook post-tool should parse");
        let Some(Command::Hook { command }) = args.command else {
            unreachable!("expected Hook command");
        };
        assert!(matches!(command, HookCommand::PostTool { .. }));
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
    fn test_cli_config_subcommand() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "config"]);
        let args = args.expect("config subcommand should parse");
        assert!(matches!(args.command, Some(Command::Config)));
    }
}
