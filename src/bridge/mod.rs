// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Pending CWD stash for grep/glob relative-pattern resolution.
pub mod cwd_stash;
/// Diagnostics pipeline for PostToolUse hook requests.
pub mod diagnostics_server;
/// In-memory editing state manager.
pub mod editing_manager;
/// Glob tool handler: unified file/directory/pattern browsing.
mod file_tools;
/// Single authority for file classification (binary, language ID, shebang).
pub mod filesystem_manager;
/// Grep tool: ripgrep + workspace/symbol pipeline with LSP enrichment.
mod grep_server;
/// Maps MCP tool calls to LSP requests.
mod handler;
/// Application dispatch for hook requests.
mod hook_router;
/// Path validation for LSP-aware operations and config file protection.
pub mod path_security;
/// Shared container for tool servers and cross-tool infrastructure.
pub mod session;
/// Transformation layer trait between protocol boundaries and LSP.
pub mod tool_server;

pub use diagnostics_server::DiagnosticsServer;
pub use editing_manager::EditingManager;
pub use handler::McpRouter;
pub use hook_router::HookRouter;
pub(crate) use hook_router::is_catenary_tool;
pub use path_security::PathValidator;
pub use tool_server::ToolServer;
