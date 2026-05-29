// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

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
/// Path utilities shared by bridge components.
mod handler;
/// Application dispatch for hook requests.
mod hook_router;
/// Shared page-based output pagination.
mod pagination;
/// Path validation for LSP-aware operations and config file protection.
pub mod path_security;
/// Single-slot paginated result cache for grep and glob servers.
mod result_cache;
/// Shared container for tool servers and cross-tool infrastructure.
pub mod session;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::DispatchMethod;
use crate::lsp::LspClientManager;
use crate::lsp::server::LspServer;
use crate::symbol_index::SymbolIndex;

pub use diagnostics_server::DiagnosticsServer;
pub use editing_guardrail::EditingGuardrail;
pub use editing_manager::EditingManager;
pub use handler::expand_tilde;
pub use hook_router::HookRouter;
pub use path_security::PathValidator;

/// Compresses a path by replacing the `$HOME` prefix with `~`.
pub(crate) fn compress_home(path: &Path) -> String {
    if let Ok(home) = std::env::var("HOME") {
        let home_path = Path::new(&home);
        if let Ok(rel) = path.strip_prefix(home_path) {
            return format!("~/{}", rel.display());
        }
    }
    path.display().to_string()
}

/// Ensures the symbol index is populated for the given files.
///
/// For each file without cached symbols, opens the document on the
/// server, requests `documentSymbol`, and feeds the response to the
/// index. Files that don't exist on disk are skipped.
///
/// `parent_id` is propagated to the LSP client so that `didOpen` and
/// `documentSymbol` traffic appears as children of the calling scope
/// in the TUI.
///
/// Shared by [`grep_server::GrepServer`] and [`file_tools::GlobServer`].
pub(super) async fn ensure_symbols(
    symbol_index: Option<&Arc<std::sync::Mutex<SymbolIndex>>>,
    client_manager: &LspClientManager,
    files: &[PathBuf],
    parent_id: Option<&str>,
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
        let Ok(uri) = client_manager
            .open_document_on(path, server, parent_id.map(str::to_string))
            .await
        else {
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
