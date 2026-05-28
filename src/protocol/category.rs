// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Protocol method categorization for LSP, MCP, and hook messages.
//!
//! Pure functions that map method strings to category labels following
//! the LSP and MCP specs' own method groupings. Used by both the core
//! layer (severity selection at emit sites) and the display layer (TUI
//! collapse and formatting).

/// Categorize an LSP method.
#[must_use]
pub fn lsp_category(method: &str) -> &'static str {
    match method {
        // lifecycle
        "initialize"
        | "initialized"
        | "shutdown"
        | "exit"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "$/setTrace"
        | "$/logTrace" => "lifecycle",

        // sync
        "textDocument/didOpen"
        | "textDocument/didChange"
        | "textDocument/didSave"
        | "textDocument/didClose"
        | "textDocument/willSave"
        | "textDocument/willSaveWaitUntil" => "sync",

        // language
        "textDocument/hover"
        | "textDocument/definition"
        | "textDocument/references"
        | "textDocument/rename"
        | "textDocument/prepareRename"
        | "textDocument/implementation"
        | "textDocument/typeDefinition"
        | "textDocument/declaration"
        | "textDocument/codeAction"
        | "textDocument/documentSymbol"
        | "textDocument/completion"
        | "textDocument/signatureHelp"
        | "textDocument/formatting"
        | "textDocument/rangeFormatting"
        | "textDocument/diagnostic"
        | "textDocument/codeLens"
        | "textDocument/documentHighlight"
        | "textDocument/foldingRange"
        | "textDocument/selectionRange"
        | "textDocument/linkedEditingRange"
        | "textDocument/semanticTokens/full"
        | "textDocument/semanticTokens/range"
        | "callHierarchy/incomingCalls"
        | "callHierarchy/outgoingCalls"
        | "textDocument/prepareCallHierarchy"
        | "typeHierarchy/subtypes"
        | "typeHierarchy/supertypes"
        | "textDocument/prepareTypeHierarchy"
        | "workspace/symbol"
        | "workspaceSymbol/resolve" => "language",

        // window
        "window/logMessage" | "window/showMessage" | "window/workDoneProgress/create" => "window",

        // workspace
        "workspace/configuration"
        | "workspace/didChangeConfiguration"
        | "workspace/didChangeWatchedFiles"
        | "workspace/didChangeWorkspaceFolders" => "workspace",

        // progress
        "$/progress" => "progress",

        _ => "unknown",
    }
}

/// Categorize an MCP method.
#[must_use]
pub fn mcp_category(method: &str) -> &'static str {
    match method {
        "initialize" | "notifications/initialized" => "init",
        "tools/list" | "tools/call" => "tools",
        "roots/list" | "notifications/roots/list_changed" => "roots",
        "notifications/cancelled" => "cancelled",
        _ => "unknown",
    }
}

/// Categorize a hook method.
///
/// Matches on the namespace prefix (before the `/`). The IPC
/// socket carries two kinds of traffic:
/// - `pre-tool/*`, `pre-agent/*`, `post-agent/*`,
///   `session-start/*`, `session-end/*` — hook lifecycle events
/// - `tool/*` — CLI commands (agent-invoked via shell tool)
#[must_use]
pub fn hook_category(method: &str) -> &'static str {
    match method.split('/').next().unwrap_or(method) {
        "tool" => "tool",
        "pre-tool" | "pre-agent" | "post-agent" | "session-start" | "session-end" => "lifecycle",
        _ => "unknown",
    }
}

/// Map an LSP category to a tracing severity level.
///
/// Language requests and progress are `info` (interesting signal).
/// Everything else (sync, lifecycle, workspace, unknown) is `debug`
/// (routine plumbing).
#[must_use]
pub fn lsp_category_level(category: &str) -> tracing::Level {
    match category {
        "language" | "progress" => tracing::Level::INFO,
        _ => tracing::Level::DEBUG,
    }
}

/// Determine the tracing level for an incoming `window/showMessage`
/// notification based on the LSP `MessageType`.
///
/// `MessageType` enum: 1=error, 2=warning, 3=info, 4=log.
#[must_use]
pub const fn window_message_level(message_type: Option<u64>) -> tracing::Level {
    match message_type {
        Some(1) => tracing::Level::ERROR,
        Some(2) => tracing::Level::WARN,
        Some(3) => tracing::Level::INFO,
        _ => tracing::Level::DEBUG,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── LSP category ────────────────────────────────────────────────────

    #[test]
    fn lsp_category_hover() {
        assert_eq!(lsp_category("textDocument/hover"), "language");
    }

    #[test]
    fn lsp_category_progress() {
        assert_eq!(lsp_category("$/progress"), "progress");
    }

    #[test]
    fn lsp_category_did_open() {
        assert_eq!(lsp_category("textDocument/didOpen"), "sync");
    }

    #[test]
    fn lsp_category_workspace_symbol_is_language() {
        assert_eq!(lsp_category("workspace/symbol"), "language");
    }

    #[test]
    fn lsp_category_unknown() {
        assert_eq!(lsp_category("custom/unknownMethod"), "unknown");
    }

    #[test]
    fn lsp_category_workspace_methods() {
        assert_eq!(lsp_category("workspace/configuration"), "workspace");
        assert_eq!(
            lsp_category("workspace/didChangeConfiguration"),
            "workspace"
        );
        assert_eq!(lsp_category("workspace/didChangeWatchedFiles"), "workspace");
        assert_eq!(
            lsp_category("workspace/didChangeWorkspaceFolders"),
            "workspace"
        );
    }

    // ── MCP category ────────────────────────────────────────────────────

    #[test]
    fn mcp_category_tools_call() {
        assert_eq!(mcp_category("tools/call"), "tools");
    }

    #[test]
    fn mcp_category_initialize() {
        assert_eq!(mcp_category("initialize"), "init");
    }

    #[test]
    fn mcp_category_roots() {
        assert_eq!(mcp_category("roots/list"), "roots");
        assert_eq!(mcp_category("notifications/roots/list_changed"), "roots");
    }

    #[test]
    fn mcp_category_cancelled() {
        assert_eq!(mcp_category("notifications/cancelled"), "cancelled");
    }

    // ── Hook category ───────────────────────────────────────────────────

    #[test]
    fn hook_category_methods() {
        // tool (CLI commands)
        assert_eq!(hook_category("tool/editing-stop"), "tool");
        assert_eq!(hook_category("tool/editing-start"), "tool");
        assert_eq!(hook_category("tool/roots-add"), "tool");
        assert_eq!(hook_category("tool/roots-rm"), "tool");
        assert_eq!(hook_category("tool/roots-ls"), "tool");
        assert_eq!(hook_category("tool/shutdown"), "tool");
        // lifecycle (hook events)
        assert_eq!(hook_category("pre-agent/turn-start"), "lifecycle");
        assert_eq!(hook_category("pre-tool/editing-state"), "lifecycle");
        assert_eq!(hook_category("pre-tool/editing-start"), "lifecycle");
        assert_eq!(hook_category("pre-tool/editing-stop"), "lifecycle");
        assert_eq!(hook_category("pre-tool/check-command"), "lifecycle");
        assert_eq!(hook_category("post-agent/require-release"), "lifecycle");
        assert_eq!(hook_category("session-start/clear-editing"), "lifecycle");
        assert_eq!(hook_category("session-end/cleanup"), "lifecycle");
        // unknown
        assert_eq!(hook_category("unknown/method"), "unknown");
    }

    // ── Level mapping ───────────────────────────────────────────────────

    #[test]
    fn lsp_category_level_language_is_info() {
        assert_eq!(lsp_category_level("language"), tracing::Level::INFO);
    }

    #[test]
    fn lsp_category_level_progress_is_info() {
        assert_eq!(lsp_category_level("progress"), tracing::Level::INFO);
    }

    #[test]
    fn lsp_category_level_sync_is_debug() {
        assert_eq!(lsp_category_level("sync"), tracing::Level::DEBUG);
    }

    #[test]
    fn lsp_category_level_lifecycle_is_debug() {
        assert_eq!(lsp_category_level("lifecycle"), tracing::Level::DEBUG);
    }

    #[test]
    fn window_message_level_error() {
        assert_eq!(window_message_level(Some(1)), tracing::Level::ERROR);
    }

    #[test]
    fn window_message_level_warn() {
        assert_eq!(window_message_level(Some(2)), tracing::Level::WARN);
    }

    #[test]
    fn window_message_level_info() {
        assert_eq!(window_message_level(Some(3)), tracing::Level::INFO);
    }

    #[test]
    fn window_message_level_log() {
        assert_eq!(window_message_level(Some(4)), tracing::Level::DEBUG);
    }

    #[test]
    fn window_message_level_none() {
        assert_eq!(window_message_level(None), tracing::Level::DEBUG);
    }
}
