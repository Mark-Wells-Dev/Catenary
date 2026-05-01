// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::filesystem_manager::FilesystemManager;

use super::tool_server::ToolServer;
use super::toolbox::Toolbox;
use crate::mcp::{CallToolResult, Tool, ToolHandler};

/// MCP tool call router.
///
/// Routes `tools/call` requests to the appropriate tool server and
/// handles editing lifecycle (`start_editing`/`done_editing`).
/// Implements [`ToolHandler`] to maintain clean dependency direction
/// between the `mcp` (protocol) and `bridge` (application) modules.
pub struct McpRouter {
    toolbox: Arc<Toolbox>,
}

impl McpRouter {
    /// Creates a new `McpRouter` wrapping a shared `Toolbox`.
    #[must_use]
    pub const fn new(toolbox: Arc<Toolbox>) -> Self {
        Self { toolbox }
    }
}

/// Expands a leading `~` or `~/` to the user's home directory.
pub(super) fn expand_tilde(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{}", &path[1..]);
    }
    path.to_string()
}

/// Resolves a file path, converting relative paths to absolute using the current working directory.
///
/// Expands a leading `~` to `$HOME` before resolution.
pub(super) fn resolve_path(file: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(file);
    let path = PathBuf::from(&expanded);
    if path.is_absolute() {
        Ok(path)
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| anyhow!("Failed to get current working directory: {e}"))?;
        Ok(cwd.join(path))
    }
}

/// Makes a file path relative to the owning root, for display.
///
/// Uses [`FilesystemManager::resolve_root`] for longest-prefix matching
/// instead of ad-hoc iteration.
pub(super) fn display_path(file: &str, fs: &FilesystemManager) -> String {
    let path = Path::new(file);
    fs.resolve_root(path).map_or_else(
        || file.to_string(),
        |root| {
            path.strip_prefix(&root).map_or_else(
                |_| file.to_string(),
                |rel| rel.to_string_lossy().to_string(),
            )
        },
    )
}

/// Resolve relative pattern parameters against the host CLI's cwd.
///
/// For grep: resolves `glob` and `exclude` params.
/// For glob: resolves `pattern` param.
///
/// A pattern is considered relative (and therefore resolved) when it does
/// not start with `/` or `~` after tilde expansion. Absolute patterns are
/// left unchanged.
fn resolve_params_against_cwd(tool: &str, params: &mut Value, cwd: &Path) {
    match tool {
        "grep" => {
            resolve_param(params, "glob", cwd);
            resolve_param(params, "exclude", cwd);
        }
        "glob" => {
            resolve_param(params, "pattern", cwd);
        }
        _ => {}
    }
}

/// Resolve a single string parameter to an absolute path if it is relative.
fn resolve_param(params: &mut Value, key: &str, cwd: &Path) {
    let Some(val) = params.get(key).and_then(Value::as_str) else {
        return;
    };
    let expanded = expand_tilde(val);
    if Path::new(&expanded).is_absolute() {
        return;
    }
    let resolved = cwd.join(&expanded);
    params[key] = Value::String(resolved.to_string_lossy().into_owned());
}

impl ToolHandler for McpRouter {
    fn list_tools(&self) -> Vec<Tool> {
        let grep_budget = self.toolbox.grep.budget;
        let glob_budget = self.toolbox.glob.budget;
        let outline_threshold = self.toolbox.glob.outline_threshold;

        vec![
            Tool {
                name: "grep".to_string(),
                title: Some("Catenary: Grep".to_string()),
                description: Some(format!(
                    "Search for a pattern across the workspace. Queries the LSP symbol index \
                     and ripgrep in parallel. Use `|` for alternation (e.g., `foo|bar`). \
                     Scope with `glob` and `exclude` to narrow the file set.\n\n\
                     Output fits a {grep_budget}-character budget. Broad queries produce more \
                     matches than the budget can show at full detail, so the tool reduces \
                     detail automatically. Narrow your pattern or add a glob to get richer \
                     results."
                )),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for (supports | for alternation)"
                        },
                        "glob": {
                            "type": "string",
                            "description": "Glob pattern to scope the search (e.g., src/**/*.rs)"
                        },
                        "exclude": {
                            "type": "string",
                            "description": "Glob pattern to exclude from matches"
                        },
                        "include_gitignored": {
                            "type": "boolean",
                            "description": "Include gitignored files (default: false)"
                        },
                        "include_hidden": {
                            "type": "boolean",
                            "description": "Include hidden files (default: false)"
                        }
                    },
                    "required": ["pattern"]
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                })),
            },
            Tool {
                name: "glob".to_string(),
                title: Some("Catenary: Glob".to_string()),
                description: Some(format!(
                    "Browse the workspace. Auto-detects intent: file path \u{2192} symbol outline, \
                     directory path \u{2192} listing with symbols, glob pattern \u{2192} matching files \
                     with symbols. Always shows outline-level symbols (structs, classes, enums, \
                     interfaces, modules, constants).\n\n\
                     Output fits a {glob_budget}-character budget. Large directories are \
                     bucketed into drillable glob patterns. Files over {outline_threshold} \
                     lines include a defensive outline \u{2014} a map of top-level symbols with \
                     line ranges. Single files always include the outline regardless of size."
                )),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "A file path, directory path, or glob pattern (e.g., 'src/', 'src/main.rs', '**/*.rs')"
                        },
                        "exclude": {
                            "type": "string",
                            "description": "Glob pattern to exclude from results"
                        },
                        "cursor": {
                            "type": "string",
                            "description": "Continuation token from previous result"
                        },
                        "include_gitignored": {
                            "type": "boolean",
                            "description": "Include files ignored by .gitignore (default: false)"
                        },
                        "include_hidden": {
                            "type": "boolean",
                            "description": "Include hidden files and directories (default: false)"
                        }
                    },
                    "required": ["pattern"]
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                })),
            },
            Tool {
                name: "start_editing".to_string(),
                title: Some("Catenary: Start Editing".to_string()),
                description: Some(
                    "Enter editing mode. Diagnostics are deferred until done_editing is called. \
                     Call this before using Edit. Takes no arguments."
                        .to_string(),
                ),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": false,
                    "destructiveHint": false,
                    "idempotentHint": true,
                    "openWorldHint": false
                })),
            },
            Tool {
                name: "done_editing".to_string(),
                title: Some("Catenary: Done Editing".to_string()),
                description: Some(
                    "Exit editing mode and return LSP diagnostics for all modified files. \
                     While editing, Edit, Read, grep, and glob remain available. All other \
                     tools are blocked until done_editing returns.\n\n\
                     Output lists every modified file as diagnostics (errors/warnings), \
                     clean, or N/A (no language server coverage). When truncated, a \
                     [cursor: ...] token appears — pass it back to see the next page."
                        .to_string(),
                ),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "cursor": {
                            "type": "string",
                            "description": "Continuation token from a previous truncated result.",
                        }
                    },
                }),
                annotations: Some(serde_json::json!({
                    "readOnlyHint": true,
                    "destructiveHint": false,
                    "idempotentHint": false,
                    "openWorldHint": false
                })),
            },
        ]
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        parent_id: Option<i64>,
        cancel: &CancellationToken,
    ) -> Result<CallToolResult> {
        // Editing tools: no-op triggers. The PreToolUse hook enters editing
        // mode (start_editing) and the PostToolUse hook exits + runs batch
        // diagnostics (done_editing). The hooks own the state transitions
        // because they have the real agent_id from the host CLI.
        if name == "start_editing" {
            self.toolbox.diagnostics.clear_cache();
            return Ok(CallToolResult::text(
                "editing mode \u{2014} diagnostics deferred until done_editing",
            ));
        }

        if name == "done_editing" {
            let cursor = arguments
                .as_ref()
                .and_then(|a| a.get("cursor"))
                .and_then(serde_json::Value::as_str);

            if let Some(token) = cursor {
                // Serve from cache — no editing state change, no LSP.
                let output = self
                    .toolbox
                    .diagnostics
                    .get_cursor(token)
                    .unwrap_or_else(|| {
                        "invalid or expired cursor \u{2014} run done_editing first".into()
                    });
                return Ok(CallToolResult::text(output));
            }

            let files = self.toolbox.editing.drain_all_and_clear();
            let entry_id = parent_id.unwrap_or(0);
            let output = self.toolbox.runtime.block_on(
                self.toolbox
                    .diagnostics
                    .process_files_batched(&files, entry_id),
            );

            return Ok(CallToolResult::text(output));
        }

        // Notify servers about filesystem changes before any LSP interaction.
        self.toolbox
            .runtime
            .block_on(self.toolbox.notify_file_changes());

        // ToolServer dispatch: grep, glob
        let mut params = arguments.unwrap_or(Value::Null);

        // Resolve relative patterns against the host CLI's working
        // directory (stashed by the PreToolUse hook). Absolute patterns
        // and tilde-prefixed patterns pass through unchanged.
        if let Some(cwd) = self.toolbox.cwd_stash.take() {
            resolve_params_against_cwd(name, &mut params, &cwd);
        }

        let result = match name {
            "grep" => self
                .toolbox
                .runtime
                .block_on(self.toolbox.grep.execute(&params, parent_id, cancel)),
            "glob" => self
                .toolbox
                .runtime
                .block_on(self.toolbox.glob.execute(&params, parent_id, cancel)),
            _ => return Err(anyhow!("Unknown tool: {name}")),
        };

        match result {
            Ok(v) => {
                let text = v.as_str().unwrap_or("").to_string();
                Ok(CallToolResult::text(text))
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn resolve_param_relative_becomes_absolute() {
        let mut params = serde_json::json!({"glob": "src/**/*.rs"});
        resolve_param(&mut params, "glob", Path::new("/home/user/project"));
        assert_eq!(
            params["glob"].as_str(),
            Some("/home/user/project/src/**/*.rs")
        );
    }

    #[test]
    fn resolve_param_absolute_unchanged() {
        let mut params = serde_json::json!({"glob": "/tmp/src/**/*.rs"});
        resolve_param(&mut params, "glob", Path::new("/home/user/project"));
        assert_eq!(params["glob"].as_str(), Some("/tmp/src/**/*.rs"));
    }

    #[test]
    fn resolve_param_tilde_unchanged() {
        let mut params = serde_json::json!({"glob": "~/projects/*.rs"});
        resolve_param(&mut params, "glob", Path::new("/home/user/project"));
        // expand_tilde expands to absolute internally → resolve_param
        // returns early. The param value stays as "~/..." because
        // downstream tools (ResolvedGlob::new, resolve_path) also
        // call expand_tilde.
        assert_eq!(
            params["glob"].as_str(),
            Some("~/projects/*.rs"),
            "tilde patterns should not be resolved against cwd"
        );
    }

    #[test]
    fn resolve_param_missing_key_is_noop() {
        let mut params = serde_json::json!({"pattern": "foo"});
        resolve_param(&mut params, "glob", Path::new("/cwd"));
        assert!(params.get("glob").is_none());
    }

    #[test]
    fn resolve_grep_resolves_glob_and_exclude() {
        let mut params = serde_json::json!({
            "pattern": "TODO",
            "glob": "src/**/*.rs",
            "exclude": "tests/**"
        });
        let cwd = Path::new("/project");
        resolve_params_against_cwd("grep", &mut params, cwd);
        assert_eq!(params["glob"].as_str(), Some("/project/src/**/*.rs"));
        assert_eq!(params["exclude"].as_str(), Some("/project/tests/**"));
        // pattern is not resolved for grep
        assert_eq!(params["pattern"].as_str(), Some("TODO"));
    }

    #[test]
    fn resolve_glob_resolves_pattern() {
        let mut params = serde_json::json!({"pattern": "src/"});
        let cwd = Path::new("/project");
        resolve_params_against_cwd("glob", &mut params, cwd);
        assert_eq!(params["pattern"].as_str(), Some("/project/src/"));
    }

    #[test]
    fn resolve_glob_does_not_resolve_exclude() {
        let mut params = serde_json::json!({"pattern": "/abs/path", "exclude": "test_*"});
        let cwd = Path::new("/project");
        resolve_params_against_cwd("glob", &mut params, cwd);
        // pattern is absolute → unchanged
        assert_eq!(params["pattern"].as_str(), Some("/abs/path"));
        // exclude is NOT resolved for glob (not in scope per ticket)
        assert_eq!(params["exclude"].as_str(), Some("test_*"));
    }

    #[test]
    fn resolve_unknown_tool_is_noop() {
        let mut params = serde_json::json!({"pattern": "relative"});
        let cwd = Path::new("/project");
        resolve_params_against_cwd("start_editing", &mut params, cwd);
        assert_eq!(params["pattern"].as_str(), Some("relative"));
    }
}
