// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bridge handler that maps MCP tool calls to LSP requests.

use anyhow::{Result, anyhow};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use super::filesystem_manager::FilesystemManager;

use super::session::Session;
use super::tool_server::ToolServer;
use crate::mcp::{CallToolResult, Tool, ToolHandler};

/// MCP tool call router.
///
/// Routes `tools/call` requests to the appropriate tool server.
/// Implements [`ToolHandler`] to maintain clean dependency direction
/// between the `mcp` (protocol) and `bridge` (application) modules.
#[derive(Clone)]
pub struct McpRouter {
    session: Arc<Session>,
}

impl McpRouter {
    /// Creates a new `McpRouter` wrapping a shared `Session`.
    #[must_use]
    pub const fn new(session: Arc<Session>) -> Self {
        Self { session }
    }
}

/// Expands a leading `~` or `~/` to the user's home directory.
#[must_use]
pub fn expand_tilde(path: &str) -> String {
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

/// Resolve relative pattern parameters against the given directory.
///
/// For grep: resolves `glob` and `exclude` params.
/// For glob: resolves `pattern` param.
///
/// A pattern is considered relative (and therefore resolved) when it does
/// not start with `/` or `~` after tilde expansion. Absolute patterns are
/// left unchanged.
fn resolve_params_against_dir(tool: &str, params: &mut Value, dir: &Path) {
    match tool {
        "grep" => {
            resolve_param(params, "glob", dir);
            resolve_param(params, "exclude", dir);
        }
        "glob" => {
            resolve_param(params, "pattern", dir);
        }
        _ => {}
    }
}

/// Resolve a single string parameter to an absolute path if it is relative.
fn resolve_param(params: &mut Value, key: &str, dir: &Path) {
    let Some(val) = params.get(key).and_then(Value::as_str) else {
        return;
    };
    let expanded = expand_tilde(val);
    if Path::new(&expanded).is_absolute() {
        return;
    }
    let resolved = dir.join(&expanded);
    params[key] = Value::String(resolved.to_string_lossy().into_owned());
}

impl ToolHandler for McpRouter {
    fn list_tools(&self) -> Vec<Tool> {
        let grep_budget = self.session.grep.budget;
        let glob_budget = self.session.glob.budget;
        let outline_threshold = self.session.glob.outline_threshold;

        vec![
            Tool {
                name: "grep".to_string(),
                title: Some("Catenary: Grep".to_string()),
                description: Some(format!(
                    "Search for a pattern across the workspace. Queries the LSP symbol index \
                     and ripgrep in parallel. Use `|` for alternation (e.g., `foo|bar`). \
                     Scope with `glob` and `exclude` to narrow the file set. Pass `directory` \
                     to set the search root for relative patterns.\n\n\
                     Output fits a {grep_budget}-character budget. When results exceed a \
                     single page, output is truncated with a `[page N/M]` header \u{2014} pass \
                     `page` to retrieve subsequent pages. Results include enriched navigation \
                     edges (calls, impls, supertypes, subtypes) when LSP symbol data is \
                     available, or a structure heatmap otherwise."
                )),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Regex pattern to search for (supports | for alternation)"
                        },
                        "directory": {
                            "type": "string",
                            "description": "Directory to search in (absolute path). Relative glob/exclude patterns resolve against this."
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
                        },
                        "page": {
                            "type": "integer",
                            "description": "Page number for paged results (default: 1). Pages re-run the query with deterministic sort order."
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
                     with symbols. Pass `directory` to set the base for relative patterns. \
                     Always shows outline-level symbols (structs, classes, enums, \
                     interfaces, modules, constants).\n\n\
                     Output fits a {glob_budget}-character page. Broad patterns produce paged \
                     results \u{2014} refine the pattern or use `page` to continue. Files over \
                     {outline_threshold} lines include a defensive outline \u{2014} a map of \
                     top-level symbols with line ranges. Single files always include the \
                     outline regardless of size."
                )),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "A file path, directory path, or glob pattern (e.g., 'src/', 'src/main.rs', '**/*.rs')"
                        },
                        "directory": {
                            "type": "string",
                            "description": "Directory to search in (absolute path). Relative patterns resolve against this."
                        },
                        "page": {
                            "type": "integer",
                            "description": "Page number for paged results (default: 1). Pages re-run the query with deterministic sort order."
                        },
                        "exclude": {
                            "type": "string",
                            "description": "Glob pattern to exclude from results"
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
        ]
    }

    fn call_tool(
        &self,
        name: &str,
        arguments: Option<serde_json::Value>,
        parent_id: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<CallToolResult> {
        // Notify servers about filesystem changes before any LSP interaction.
        self.session
            .runtime
            .block_on(self.session.notify_file_changes());

        // ToolServer dispatch: grep, glob
        let mut params = arguments.unwrap_or(Value::Null);

        // Resolve relative patterns against the explicit `directory`
        // parameter from the tool arguments.
        let dir = params
            .get("directory")
            .and_then(Value::as_str)
            .map(|d| PathBuf::from(expand_tilde(d)));

        if let Some(dir) = &dir {
            resolve_params_against_dir(name, &mut params, dir);
        }

        // `directory` is not a tool-server parameter — strip before dispatch.
        if let Some(obj) = params.as_object_mut() {
            obj.remove("directory");
        }

        let pid_ref = parent_id.as_deref();
        let result = match name {
            "grep" => self
                .session
                .runtime
                .block_on(self.session.grep.execute(&params, pid_ref, cancel)),
            "glob" => self
                .session
                .runtime
                .block_on(self.session.glob.execute(&params, pid_ref, cancel)),
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
            "tilde patterns should not be resolved against dir"
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
        let dir = Path::new("/project");
        resolve_params_against_dir("grep", &mut params, dir);
        assert_eq!(params["glob"].as_str(), Some("/project/src/**/*.rs"));
        assert_eq!(params["exclude"].as_str(), Some("/project/tests/**"));
        // pattern is not resolved for grep
        assert_eq!(params["pattern"].as_str(), Some("TODO"));
    }

    #[test]
    fn resolve_glob_resolves_pattern() {
        let mut params = serde_json::json!({"pattern": "src/"});
        let dir = Path::new("/project");
        resolve_params_against_dir("glob", &mut params, dir);
        assert_eq!(params["pattern"].as_str(), Some("/project/src/"));
    }

    #[test]
    fn resolve_glob_does_not_resolve_exclude() {
        let mut params = serde_json::json!({"pattern": "/abs/path", "exclude": "test_*"});
        let dir = Path::new("/project");
        resolve_params_against_dir("glob", &mut params, dir);
        // pattern is absolute → unchanged
        assert_eq!(params["pattern"].as_str(), Some("/abs/path"));
        // exclude is NOT resolved for glob (not in scope per ticket)
        assert_eq!(params["exclude"].as_str(), Some("test_*"));
    }

    #[test]
    fn resolve_unknown_tool_is_noop() {
        let mut params = serde_json::json!({"pattern": "relative"});
        let dir = Path::new("/project");
        resolve_params_against_dir("start_editing", &mut params, dir);
        assert_eq!(params["pattern"].as_str(), Some("relative"));
    }

    #[test]
    fn grep_with_directory_resolves_relative_glob() {
        let mut params = serde_json::json!({
            "pattern": "TODO",
            "directory": "/search/root",
            "glob": "src/**/*.rs"
        });
        let dir = params
            .get("directory")
            .and_then(Value::as_str)
            .map(|d| PathBuf::from(expand_tilde(d)));
        if let Some(dir) = &dir {
            resolve_params_against_dir("grep", &mut params, dir);
        }
        if let Some(obj) = params.as_object_mut() {
            obj.remove("directory");
        }
        assert_eq!(params["glob"].as_str(), Some("/search/root/src/**/*.rs"));
        assert!(
            params.get("directory").is_none(),
            "directory should be stripped before dispatch"
        );
    }

    #[test]
    fn glob_with_directory_resolves_relative_pattern() {
        let mut params = serde_json::json!({
            "pattern": "src/",
            "directory": "/workspace"
        });
        let dir = params
            .get("directory")
            .and_then(Value::as_str)
            .map(|d| PathBuf::from(expand_tilde(d)));
        if let Some(dir) = &dir {
            resolve_params_against_dir("glob", &mut params, dir);
        }
        if let Some(obj) = params.as_object_mut() {
            obj.remove("directory");
        }
        assert_eq!(params["pattern"].as_str(), Some("/workspace/src/"));
        assert!(
            params.get("directory").is_none(),
            "directory should be stripped before dispatch"
        );
    }
}
