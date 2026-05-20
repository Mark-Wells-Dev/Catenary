// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

/// Pending CWD stash for grep/glob relative-pattern resolution.
pub mod cwd_stash;
/// Diagnostics pipeline for PostToolUse hook requests.
pub mod diagnostics_server;
/// Cross-session per-root editing guardrail.
pub mod editing_guardrail;
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
/// Shared page-based output pagination.
mod pagination;
/// Path validation for LSP-aware operations and config file protection.
pub mod path_security;
/// Shared container for tool servers and cross-tool infrastructure.
pub mod session;
/// Transformation layer trait between protocol boundaries and LSP.
pub mod tool_server;

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::DispatchMethod;
use crate::lsp::LspClientManager;
use crate::lsp::server::LspServer;
use crate::symbol_index::SymbolIndex;

pub use diagnostics_server::DiagnosticsServer;
pub use editing_guardrail::EditingGuardrail;
pub use editing_manager::EditingManager;
pub use handler::McpRouter;
pub use hook_router::HookRouter;
pub(crate) use hook_router::is_catenary_tool;
pub use path_security::PathValidator;
pub use tool_server::ToolServer;

/// Ensures the symbol index is populated for the given files.
///
/// For each file without cached symbols, opens the document on the
/// server, requests `documentSymbol`, and feeds the response to the
/// index. Files that don't exist on disk are skipped.
///
/// Shared by [`grep_server::GrepServer`] and [`file_tools::GlobServer`].
pub(super) async fn ensure_symbols(
    symbol_index: Option<&Arc<std::sync::Mutex<SymbolIndex>>>,
    client_manager: &LspClientManager,
    files: &[PathBuf],
) {
    let Some(idx_arc) = symbol_index else {
        return;
    };
    let needs_populate: Vec<PathBuf> = {
        let Ok(idx) = idx_arc.lock() else { return };
        idx.needs_symbols(files)
            .into_iter()
            .filter(|p| p.is_file())
            .cloned()
            .collect()
    };

    for path in &needs_populate {
        let servers = client_manager
            .get_servers(
                path,
                LspServer::supports_document_symbols,
                Some(DispatchMethod::DocumentSymbol),
            )
            .await;
        let Some(server) = servers.first() else {
            continue;
        };
        let Ok(uri) = client_manager.open_document_on(path, server, None).await else {
            continue;
        };
        let Ok(response) = server.lock().await.document_symbols(&uri).await else {
            continue;
        };
        if let Ok(idx) = idx_arc.lock() {
            let _ = idx.populate_from_document_symbols(path, &response);
        }
    }
}
