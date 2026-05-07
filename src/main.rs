// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Catenary MCP server and CLI.
//!
//! This is the main entry point for the Catenary multiplexing bridge.
//! It can be run as an MCP server or as a CLI tool to list and monitor sessions.

#![allow(clippy::print_stdout, reason = "CLI tool needs to output to stdout")]
#![allow(clippy::print_stderr, reason = "CLI tool needs to output to stderr")]

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use catenary_mcp::bridge::McpRouter;
use catenary_mcp::cli::{self, HostFormat, QueryFormat};
use catenary_mcp::logging::LoggingServer;
use catenary_mcp::session;

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

    /// Run as the Catenary daemon (internal, spawned by bridge proxy).
    #[command(hide = true)]
    Daemon,

    /// Stop the running Catenary daemon.
    Stop,
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
    /// `SessionEnd`: clean up session state (roots, editing).
    #[command(name = "session-end")]
    SessionEnd {
        /// Output format: "claude" or "gemini".
        #[arg(long, value_enum)]
        format: HostFormat,
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
    let args = Args::parse();

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
            let rt = build_runtime()?;
            server.map_or_else(
                || rt.block_on(cli::doctor::run_doctor(&root, nocolor, diff)),
                |server_name| {
                    rt.block_on(cli::doctor::run_doctor_single(&server_name, &root, nocolor))
                },
            )
        }
        Some(Command::Hook { command }) => {
            match command {
                HookCommand::PreAgent { format } => cli::hooks::run_pre_agent(format),
                HookCommand::PreTool { format } => cli::hooks::run_pre_tool(format),
                HookCommand::PostTool { format } => cli::hooks::run_post_tool(format),
                HookCommand::PostAgent { format } => cli::hooks::run_post_agent(format),
                HookCommand::SessionStart { format } => cli::hooks::run_session_start(format),
                HookCommand::SessionEnd { format: _ } => cli::hooks::run_session_end(),
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
        #[cfg(unix)]
        Some(Command::Daemon) => run_daemon(),
        #[cfg(not(unix))]
        Some(Command::Daemon) => Err(anyhow::anyhow!("daemon mode requires Unix")),
        #[cfg(unix)]
        Some(Command::Stop) => build_runtime()?.block_on(run_stop()),
        #[cfg(not(unix))]
        Some(Command::Stop) => Err(anyhow::anyhow!("daemon mode requires Unix")),
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

    /// Tool handler that exposes no tools (disabled workspace).
    struct DaemonDisabledHandler;
    impl catenary_mcp::mcp::ToolHandler for DaemonDisabledHandler {
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
    tracing_subscriber::registry().with(logging.clone()).init();

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

    let (handler, shared_session, shared_conn) = if disabled {
        info!("Catenary disabled by .catenary.toml (lsp = false) in {workspace_display}");
        let handler: Arc<dyn catenary_mcp::mcp::ToolHandler> = Arc::new(DaemonDisabledHandler);
        (handler, None, None)
    } else {
        let conn = catenary_mcp::db::open_and_migrate()?;
        let instance_id: Arc<str> = "daemon".into();

        // Insert a session row so MessageDbSink's FK constraint
        // (messages.session_id → sessions.id) is satisfied. Without
        // this, every tracing event after activate() triggers an FK
        // violation → trace!() → recursive on_event → stack overflow.
        let started_at = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR IGNORE INTO sessions \
             (id, pid, display_name, started_at, alive) \
             VALUES (?1, ?2, ?3, ?4, 1)",
            rusqlite::params![
                &*instance_id,
                std::process::id(),
                &workspace_display,
                &started_at
            ],
        )
        .context("insert daemon session row")?;

        let conn = Arc::new(std::sync::Mutex::new(conn));

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

        let session = Arc::new(catenary_mcp::bridge::session::Session::new(
            config,
            roots,
            logging.clone(),
            conn.clone(),
            instance_id,
            rt.handle().clone(),
            notification_router,
        ));

        // Spawn LSP servers in the background.
        let session_for_spawn = session.clone();
        rt.spawn(async move { session_for_spawn.spawn_all().await });

        let handler: Arc<dyn catenary_mcp::mcp::ToolHandler> =
            Arc::new(McpRouter::new(session.clone()));
        (handler, Some(session), Some(conn))
    };

    let session_for_shutdown = shared_session.clone();

    let manager = SessionManager::from_sockets(sockets, handler, logging);
    let manager = match (shared_session, shared_conn) {
        (Some(session), Some(conn)) => manager.with_session(session, conn),
        _ => manager,
    };

    info!(
        source = Source::DaemonLifecycle.as_str(),
        "daemon serving workspace: {workspace_display}",
    );

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
    }

    // Drop removes socket files.
    drop(manager);

    info!(source = Source::DaemonLifecycle.as_str(), "daemon stopped",);

    result
}

/// Stops the running Catenary daemon.
///
/// Connects to the daemon's hook socket and sends a shutdown request.
/// If no daemon is running, prints a message and returns successfully.
///
/// # Errors
///
/// Returns an error if the shutdown request fails after connecting.
#[cfg(unix)]
async fn run_stop() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let hook_path = catenary_mcp::router::hook_socket_path();

    let Ok(stream) = tokio::net::UnixStream::connect(&hook_path).await else {
        println!("No daemon running");
        return Ok(());
    };

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({"method": "shutdown"});
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    println!("Daemon stopped");
    Ok(())
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
    fn test_cli_config_subcommand() {
        use clap::Parser;
        let args = Args::try_parse_from(["catenary", "config"]);
        let args = args.expect("config subcommand should parse");
        assert!(matches!(args.command, Some(Command::Config)));
    }
}
