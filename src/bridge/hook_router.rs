// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application dispatch for hook requests.
//!
//! `HookRouter` owns all hook method handlers and application logic
//! (editing state enforcement, diagnostics dispatch, turn tracking).
//! Mirrors the [`super::handler::McpRouter`] pattern: protocol boundary
//! delegates to router, router delegates to [`super::session::Session`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::debug;

use crate::source::Source;

use super::session::Session;
use crate::hook::response::SystemMessageBuilder;
use crate::hook::{HookRequest, HookResult};

/// Parse a `HostFormat` from a string value sent over IPC.
fn parse_host_format(s: &str) -> Option<crate::cli::HostFormat> {
    match s {
        "claude" => Some(crate::cli::HostFormat::Claude),
        "gemini" => Some(crate::cli::HostFormat::Gemini),
        _ => None,
    }
}

// ── Tool classification helpers ─────────────────────────────────────────

/// Returns `true` if the tool is an edit tool that requires `start_editing`.
///
/// Checks all known edit tool names across host CLIs (Claude Code and Gemini CLI).
fn is_edit_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Edit" | "Write" | "NotebookEdit" | "write_file" | "replace"
    )
}

/// Returns `true` if the tool is a read tool (always allowed during editing).
///
/// Checks all known read tool names across host CLIs.
fn is_read_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Read" | "NotebookRead" | "read_file")
}

/// Returns `true` if the tool is a shell tool (Bash or `run_shell_command`).
fn is_bash_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Bash" | "run_shell_command")
}

/// Filesystem-manipulation commands allowed during editing mode.
///
/// These commands modify the filesystem without producing code changes that
/// need LSP diagnostics. Blocking them during editing forces the agent to
/// exit editing mode mid-refactor just to delete a removed module file.
const FILESYSTEM_COMMANDS: &[&str] = &["rm", "cp", "mv", "mkdir", "rmdir", "touch", "chmod", "ln"];

/// Returns `true` if a shell command contains only filesystem operations.
///
/// Uses the command parsing infrastructure from [`crate::cli::command_filter`]
/// (pipeline splitting, subshell recursion, env-var prefix stripping) to
/// extract every command name, then checks that each one is in the
/// [`FILESYSTEM_COMMANDS`] allowlist.
fn is_filesystem_only_bash(command: &str) -> bool {
    let names = crate::cli::command_filter::extract_command_names(command);
    !names.is_empty()
        && names
            .iter()
            .all(|n| FILESYSTEM_COMMANDS.contains(&n.as_str()))
}

/// Returns `true` if the tool is always allowed during editing mode.
///
/// Catenary editing tools (`start_editing`, `done_editing`) must be allowed
/// so the agent can manage editing state. `ToolSearch` must be allowed
/// because both editing tools are deferred in Claude Code — blocking
/// `ToolSearch` while editing creates an unrecoverable state if the agent
/// loaded `start_editing` but not `done_editing` before entering editing mode.
/// Catenary's `grep` and `glob` are read-only search tools that don't
/// produce diagnostics — blocking them during editing is unnecessary friction.
fn is_allowed_during_editing(tool_name: &str) -> bool {
    is_catenary_tool(tool_name, "start_editing")
        || is_catenary_tool(tool_name, "done_editing")
        || is_catenary_tool(tool_name, "grep")
        || is_catenary_tool(tool_name, "glob")
        || tool_name == "ToolSearch"
}

/// Matches Catenary tool names: bare `{suffix}` or MCP-qualified
/// `mcp*catenary*{suffix}` (Claude Code, Gemini CLI).
pub fn is_catenary_tool(tool_name: &str, suffix: &str) -> bool {
    tool_name == suffix
        || (tool_name.starts_with("mcp")
            && tool_name.contains("catenary")
            && tool_name.ends_with(suffix))
}

/// Result of hook dispatch: the handler's result plus an optional
/// `systemMessage` from the notification queue drain.
pub struct DispatchResult {
    /// Handler result (`None` = allow / no actionable data).
    pub result: Option<HookResult>,
    /// Composed `systemMessage` content (direct + background drain).
    pub system_message: Option<String>,
    /// Roots discovered from transcript scanning (`PreAgent` only).
    pub add_roots: Vec<PathBuf>,
}

// ── Transcript scanning ───────────────────────────────────────────────

/// Prefix of the `/add-dir` confirmation message in Claude Code's JSONL
/// transcript. ANSI bold escape is JSON-encoded as `\u001b[1m`.
const ADD_DIR_PREFIX: &str = "Added \\u001b[1m";

/// Suffix of the `/add-dir` confirmation message.
const ADD_DIR_SUFFIX: &str = "\\u001b[22m as a working directory";

/// Scan the transcript for `/add-dir` confirmation messages.
///
/// Reads from the byte offset stored on `session`, scans new lines for
/// the confirmation pattern, updates the offset, and returns newly
/// discovered root paths. Returns an empty vec if no transcript path is
/// stashed, the file is unreadable, or no new `/add-dir` entries exist.
fn scan_transcript(session: &Session) -> Vec<PathBuf> {
    use std::io::{BufRead, Seek, SeekFrom};

    let path = match session.transcript_path.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(p) => p.clone(),
            None => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };

    let offset = session
        .transcript_offset
        .load(std::sync::atomic::Ordering::Acquire);

    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            debug!(
                source = Source::HookDispatch.as_str(),
                "transcript scan: cannot open {}: {e}",
                path.display(),
            );
            return Vec::new();
        }
    };

    let file_len = file.metadata().map_or(0, |m| m.len());
    debug!(
        source = Source::HookDispatch.as_str(),
        offset, file_len, "transcript scan: starting",
    );

    if file.seek(SeekFrom::Start(offset)).is_err() {
        return Vec::new();
    }

    let mut roots = Vec::new();
    let reader = std::io::BufReader::new(&mut file);

    for line in reader.lines() {
        let Ok(line) = line else { break };
        if !line.contains(ADD_DIR_PREFIX) {
            continue;
        }
        let mut search_from = 0;
        while let Some(start) = line[search_from..].find(ADD_DIR_PREFIX) {
            let abs_start = search_from + start + ADD_DIR_PREFIX.len();
            if let Some(end) = line[abs_start..].find(ADD_DIR_SUFFIX) {
                let path_str = &line[abs_start..abs_start + end];
                // Unescape JSON string escapes (path is inside a JSON string)
                let path_str = path_str
                    .replace("\\\\", "\\")
                    .replace("\\/", "/")
                    .replace("\\\"", "\"");
                let path = PathBuf::from(&path_str);
                if path.is_absolute() {
                    match path.canonicalize() {
                        Ok(canonical) => {
                            debug!(
                                source = Source::HookDispatch.as_str(),
                                root = %canonical.display(),
                                "transcript scan: found /add-dir root",
                            );
                            if !roots.contains(&canonical) {
                                roots.push(canonical);
                            }
                        }
                        Err(e) => {
                            debug!(
                                source = Source::HookDispatch.as_str(),
                                "transcript scan: cannot canonicalize {}: {e}",
                                path.display(),
                            );
                        }
                    }
                }
                search_from = abs_start + end + ADD_DIR_SUFFIX.len();
            } else {
                break;
            }
        }
    }

    let new_offset = file.stream_position().unwrap_or(offset);
    session
        .transcript_offset
        .store(new_offset, std::sync::atomic::Ordering::Release);

    debug!(
        source = Source::HookDispatch.as_str(),
        roots_found = roots.len(),
        new_offset,
        "transcript scan: complete",
    );

    roots
}

// ── HookRouter ──────────────────────────────────────────────────────────

/// Application dispatch for hook requests.
///
/// Routes parsed [`HookRequest`] values to the appropriate handler and
/// returns an optional [`HookResult`]. Holds all shared application state
/// needed by hook handlers: editing state (via [`super::editing_manager::EditingManager`]
/// on [`Session`]) and a turn counter for per-turn debounce.
pub struct HookRouter {
    pub(crate) session: Arc<Session>,
    turn_counter: AtomicU64,
    /// Last turn number where a full config dump was shown on denial.
    last_config_dump_turn: AtomicU64,
    /// Config version at the time of the last full dump.
    last_config_dump_version: AtomicU64,
    conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
    instance_id: Arc<str>,
    /// Host CLI client name (e.g., `"host"`, `"claude-code"`).
    pub(crate) client_name: String,
}

impl HookRouter {
    /// Creates a new `HookRouter`.
    #[must_use]
    pub const fn new(
        session: Arc<Session>,
        conn: Arc<std::sync::Mutex<rusqlite::Connection>>,
        instance_id: Arc<str>,
        client_name: String,
    ) -> Self {
        Self {
            session,
            turn_counter: AtomicU64::new(0),
            // Initialized to MAX so the first denial always triggers a full
            // config dump (turn 0 != MAX).
            last_config_dump_turn: AtomicU64::new(u64::MAX),
            last_config_dump_version: AtomicU64::new(u64::MAX),
            conn,
            instance_id,
            client_name,
        }
    }

    /// Returns the current turn number.
    ///
    /// Incremented each time the pre-agent hook fires (once per user
    /// prompt / agent turn). Used by command filtering for per-turn debounce.
    #[cfg(test)]
    pub(crate) fn turn(&self) -> u64 {
        self.turn_counter.load(Ordering::Acquire)
    }

    /// Bump the config version counter.
    ///
    /// Delegates to `Session::config_version`. Forces the next denial
    /// to show a full config dump regardless of turn.
    #[cfg(test)]
    pub(crate) fn bump_config_version(&self) {
        self.session.config_version.fetch_add(1, Ordering::AcqRel);
    }

    /// Check whether the next command denial should show a full config dump.
    ///
    /// Returns `true` if the full dump is needed (first denial in a new turn
    /// or config version changed), `false` for a short message.
    fn should_show_full_dump(&self) -> bool {
        let current_turn = self.turn_counter.load(Ordering::Acquire);
        let current_version = self.session.config_version.load(Ordering::Acquire);
        let last_dump_turn = self.last_config_dump_turn.load(Ordering::Acquire);
        let last_dump_version = self.last_config_dump_version.load(Ordering::Acquire);

        if current_turn != last_dump_turn || current_version != last_dump_version {
            self.last_config_dump_turn
                .store(current_turn, Ordering::Release);
            self.last_config_dump_version
                .store(current_version, Ordering::Release);
            true
        } else {
            false
        }
    }

    /// Evaluate a shell command against the session's merged allowlist.
    ///
    /// Builds the merged `ResolvedCommands` from user config + all project
    /// configs for current roots. If the command is denied, applies debounce:
    /// full config dump on the first denial in a turn, short message on
    /// subsequent denials.
    fn handle_check_command(
        &self,
        command: &str,
        cwd: Option<&str>,
        format: Option<crate::cli::HostFormat>,
    ) -> DispatchResult {
        let Some(resolved) = self.session.merged_commands() else {
            return DispatchResult {
                result: None,
                system_message: None,
                add_roots: Vec::new(),
            };
        };

        if !resolved.is_active() {
            return DispatchResult {
                result: None,
                system_message: None,
                add_roots: Vec::new(),
            };
        }

        let cwd_path = cwd.map(std::path::Path::new);
        let Some(denial) = crate::cli::command_filter::check_command(command, &resolved, cwd_path)
        else {
            return DispatchResult {
                result: None,
                system_message: None,
                add_roots: Vec::new(),
            };
        };

        // Resolve build guidance with full cwd context. Prefer the effective
        // cwd from command parsing (accounts for `cd` within the command) over
        // the raw hook cwd.
        let effective_cwd_str = denial
            .effective_cwd
            .as_ref()
            .map(|p| p.display().to_string());
        let resolved_cwd = effective_cwd_str.as_deref().or(cwd);
        let build_hint = self.resolve_build_hint(&denial.command, &resolved, resolved_cwd);

        let message = if self.should_show_full_dump() {
            crate::cli::command_filter::format_denial_full(
                &denial.command,
                &resolved,
                &denial,
                format,
                build_hint.as_deref(),
            )
        } else {
            crate::cli::command_filter::format_denial_short(
                &denial.command,
                &denial,
                &resolved,
                format,
                build_hint.as_deref(),
            )
        };

        DispatchResult {
            result: Some(HookResult::Deny(message)),
            system_message: None,
            add_roots: Vec::new(),
        }
    }

    /// Resolve build guidance for a denied command using session context.
    ///
    /// Constructs a [`BuildContext`] from the session's config state and the
    /// hook's `cwd`, then resolves the `BuildGuidance` templates. Returns
    /// `None` when the denied command has no build guidance entry.
    fn resolve_build_hint(
        &self,
        denied_cmd: &str,
        resolved: &crate::config::ResolvedCommands,
        cwd: Option<&str>,
    ) -> Option<String> {
        let lookup = denied_cmd.split_whitespace().next().unwrap_or(denied_cmd);
        let crate::config::GuidanceEntry::Build(bg) = resolved.guidance_for(lookup)? else {
            return None;
        };

        // User config path — first source in the standard config chain.
        let user_config_path = crate::config::config_sources()
            .first()
            .map(|p| p.display().to_string());
        let user_path_str = user_config_path.as_deref().unwrap_or("user config");

        // Project config state for the cwd's root.
        let cwd_path = cwd.map(std::path::Path::new);
        let project_commands = self.session.client_manager.project_commands();
        let roots = self.session.client_manager.roots();

        // Find the root matching cwd (longest prefix).
        let matching_root = cwd_path.and_then(|cwd| {
            roots
                .iter()
                .filter(|r| cwd.starts_with(r))
                .max_by_key(|r| r.as_os_str().len())
        });

        let has_project = matching_root.is_some();
        let project_build_owned = matching_root
            .and_then(|r| project_commands.get(r))
            .and_then(|cmds| cmds.build.as_ref())
            .map_or(&[] as &[String], |sv| &sv.0);
        let project_path = matching_root.map(|r| r.join(".catenary.toml").display().to_string());

        let ctx = crate::config::BuildContext {
            user_config_path: user_path_str,
            default_build: &resolved.default_build,
            has_project_config: has_project,
            project_config_path: project_path.as_deref(),
            project_build: project_build_owned,
            cwd_resolved: cwd.is_some(),
            resolved_cwd_path: cwd,
        };

        Some(bg.resolve(&ctx))
    }

    /// Dispatches a parsed hook request to the appropriate handler.
    ///
    /// Returns a [`DispatchResult`] with the handler's result and an optional
    /// `systemMessage` from the notification queue drain. The queue is drained
    /// only at stationary points (`SessionStart`, `Stop`/`AfterAgent` when allowing).
    #[allow(clippy::too_many_lines, reason = "match arms are sequential and flat")]
    pub(crate) fn dispatch(&self, request: HookRequest, _entry_id: i64) -> DispatchResult {
        match request {
            HookRequest::PreAgent {
                transcript_path,
                session_id,
            } => {
                self.store_client_session_id(session_id.as_deref());
                let turn = self.turn_counter.fetch_add(1, Ordering::AcqRel) + 1;
                debug!(
                    source = Source::HookDispatch.as_str(),
                    turn, "Hook: turn start"
                );
                self.session
                    .roots_refresh_requested
                    .store(true, Ordering::Release);

                // Stash transcript path for scanning.
                if let Some(path) = transcript_path
                    && let Ok(mut tp) = self.session.transcript_path.lock()
                {
                    *tp = Some(PathBuf::from(path));
                }

                // Scan transcript for /add-dir roots and store them.
                let new_roots = scan_transcript(&self.session);
                if !new_roots.is_empty()
                    && let Ok(mut stored) = self.session.transcript_roots.lock()
                {
                    for root in &new_roots {
                        if !stored.contains(root) {
                            stored.push(root.clone());
                        }
                    }
                }

                DispatchResult {
                    result: None,
                    system_message: None,
                    add_roots: new_roots,
                }
            }
            HookRequest::PreTool {
                tool_name,
                file_path,
                command,
                agent_id,
                session_id,
                cwd,
            } => {
                self.store_client_session_id(session_id.as_deref());
                let result = self.handle_enforce_editing(
                    &tool_name,
                    file_path.as_deref(),
                    command.as_deref(),
                    session_id.as_deref(),
                    &agent_id,
                );
                // File tracking: accumulate when allowed and editing.
                // Runs alongside PostToolUse accumulation during the
                // transition period — EditingManager::add_file deduplicates.
                if result.is_none()
                    && let Some(ref path) = file_path
                {
                    self.handle_file_accumulation(
                        path,
                        session_id.as_deref(),
                        &agent_id,
                        Some(&tool_name),
                    );
                }
                // Stash the scope parent UUID so the MCP tools/call and
                // post-tool hook events can reference this pre-tool hook
                // as their scope root. Only when the tool is allowed —
                // denied tools produce no MCP call or post-tool hook to
                // consume the stash.
                if result.is_none() {
                    let scope_uuid = uuid::Uuid::new_v4().to_string();
                    self.session.scope_id_stash.stash(scope_uuid);
                }
                // Stash the host CLI's cwd for the upcoming MCP grep/glob
                // call, but only when the tool is allowed (denied tools
                // won't produce an MCP call, so stashing would leave a
                // stale entry).
                if result.is_none()
                    && let Some(cwd_str) = cwd
                    && (is_catenary_tool(&tool_name, "grep")
                        || is_catenary_tool(&tool_name, "glob"))
                {
                    self.session
                        .cwd_stash
                        .stash(std::path::PathBuf::from(cwd_str));
                }
                DispatchResult {
                    result,
                    system_message: None,
                    add_roots: Vec::new(),
                }
            }
            HookRequest::PreToolStartEditing {
                agent_id,
                session_id,
            } => {
                self.store_client_session_id(session_id.as_deref());
                let _ = self
                    .session
                    .editing
                    .start_editing(session_id.as_deref(), &agent_id);
                DispatchResult {
                    result: None,
                    system_message: None,
                    add_roots: Vec::new(),
                }
            }
            HookRequest::CheckCommand {
                command,
                cwd,
                session_id,
                format,
            } => {
                self.store_client_session_id(session_id.as_deref());
                let host_format = format.as_deref().and_then(parse_host_format);
                self.handle_check_command(&command, cwd.as_deref(), host_format)
            }
            HookRequest::PostTool {
                file,
                tool,
                agent_id,
                session_id,
            } => {
                self.store_client_session_id(session_id.as_deref());
                debug!(
                    source = Source::HookDispatch.as_str(),
                    "Hook: processing file {file}"
                );
                DispatchResult {
                    result: self.handle_file_accumulation(
                        &file,
                        session_id.as_deref(),
                        &agent_id,
                        tool.as_deref(),
                    ),
                    system_message: None,
                    add_roots: Vec::new(),
                }
            }
            HookRequest::PostAgent {
                agent_id,
                session_id,
                stop_hook_active,
            } => {
                self.store_client_session_id(session_id.as_deref());
                let result =
                    self.handle_require_release(session_id.as_deref(), &agent_id, stop_hook_active);
                // Drain at stationary point: only when allowing the stop.
                let system_message = if matches!(result, Some(HookResult::Block(_))) {
                    None
                } else {
                    self.drain_notifications()
                };
                DispatchResult {
                    result,
                    system_message,
                    add_roots: Vec::new(),
                }
            }
            HookRequest::SessionStart { session_id } => {
                self.store_client_session_id(session_id.as_deref());
                let result = self.handle_clear_editing();
                // Drain at stationary point: session start.
                let system_message = self.drain_notifications();
                DispatchResult {
                    result,
                    system_message,
                    add_roots: Vec::new(),
                }
            }
            HookRequest::SessionEnd { session_id } => {
                self.store_client_session_id(session_id.as_deref());
                // No-op at the router level — cleanup happens in the
                // daemon's handle_hook_dispatch (root tracker removal).
                DispatchResult {
                    result: None,
                    system_message: None,
                    add_roots: Vec::new(),
                }
            }
        }
    }

    /// Drain the notification queue into a `systemMessage` string.
    ///
    /// Drains from the shared [`crate::logging::notification_router::NotificationRouter`]
    /// using this session's `session_id`.
    ///
    /// Returns `None` if the queue is empty.
    fn drain_notifications(&self) -> Option<String> {
        let mut builder = SystemMessageBuilder::new();
        for notification in &self
            .session
            .notification_router
            .drain(&self.session.instance_id)
        {
            builder.push_background(notification.format());
        }
        builder.finish()
    }

    // ── Hook handlers ───────────────────────────────────────────────────

    /// Editing state enforcement: deny or allow a tool call.
    ///
    /// If the agent is in editing mode, only Edit/Read/Write, Catenary
    /// editing tools, and filesystem-only Bash commands are allowed. If the
    /// agent is not in editing mode, Edit/Write requires `start_editing`
    /// first.
    ///
    /// When the tool is `start_editing`, enters editing mode as a side effect
    /// (the MCP tool is a trigger — the hook owns the state transition
    /// because it has the real `agent_id` from the host CLI).
    fn handle_enforce_editing(
        &self,
        tool_name: &str,
        file_path: Option<&str>,
        command: Option<&str>,
        session_id: Option<&str>,
        agent_id: &str,
    ) -> Option<HookResult> {
        // start_editing: enter editing mode and allow unconditionally.
        // The cross-session guardrail is checked lazily at Edit/Write
        // time (below), when the file path reveals the actual root.
        if is_catenary_tool(tool_name, "start_editing") {
            let _ = self.session.editing.start_editing(session_id, agent_id);
            return None;
        }

        let agent_editing = self.session.editing.is_editing(session_id, agent_id);

        if agent_editing {
            if is_edit_tool(tool_name) {
                // Check cross-session guardrail on the file's root.
                // Locks are acquired lazily per-root, so only roots
                // with actual edits are locked.
                if let Some(guardrail) = &self.session.editing_guardrail
                    && let Some(root) = file_path
                        .map(Path::new)
                        .and_then(|p| self.session.resolve_root(p))
                    && let Err(msg) = guardrail.try_acquire(&root, &self.session.instance_id)
                {
                    return Some(HookResult::Deny(msg));
                }
                None
            } else if is_allowed_during_editing(tool_name)
                || is_read_tool(tool_name)
                || (is_bash_tool(tool_name) && command.is_some_and(is_filesystem_only_bash))
            {
                None
            } else {
                Some(HookResult::Deny(
                    "call done_editing to get diagnostics".into(),
                ))
            }
        } else if is_edit_tool(tool_name) {
            // Skip the editing gate for files without known LSP coverage.
            // In-root files always have coverage. Out-of-root files have
            // coverage only after a single-file server has successfully
            // initialized (positive cache). Files with no cache entry or
            // a negative cache entry skip the gate — no diagnostics would
            // be produced, so requiring start_editing is pointless.
            if file_path.is_some_and(|p| !self.session.has_lsp_coverage(Path::new(p))) {
                return None;
            }
            Some(HookResult::Deny("call start_editing before editing".into()))
        } else {
            None
        }
    }

    /// Accumulates edited file paths during editing mode.
    ///
    /// When the agent is in editing mode and the tool is an edit tool,
    /// accumulates the file path for later batch diagnostics in
    /// `done_editing`. Always returns `None` — diagnostics are produced
    /// only by the MCP `done_editing` tool result.
    fn handle_file_accumulation(
        &self,
        file_path: &str,
        session_id: Option<&str>,
        agent_id: &str,
        tool_name: Option<&str>,
    ) -> Option<HookResult> {
        if self.session.editing.is_editing(session_id, agent_id)
            && tool_name.is_some_and(is_edit_tool)
        {
            // Only accumulate files with known LSP coverage — files
            // without coverage have no server to produce diagnostics,
            // so processing them in done_editing is wasted work.
            let path = Path::new(file_path);
            if self.session.has_lsp_coverage(path) {
                self.session
                    .editing
                    .add_file(session_id, agent_id, PathBuf::from(file_path));
            }
        }
        None
    }

    /// Force `done_editing` before the agent stops.
    ///
    /// If `stop_hook_active` is true (retry after the agent failed to call
    /// `done_editing`), force-clears the stale editing state and allows.
    /// Otherwise blocks if the agent is in editing mode.
    fn handle_require_release(
        &self,
        session_id: Option<&str>,
        agent_id: &str,
        stop_hook_active: bool,
    ) -> Option<HookResult> {
        if stop_hook_active {
            // Agent was told to call done_editing but didn't. Clear stale
            // state rather than leaving it for SessionStart/GC cleanup.
            self.session.editing.done_editing(session_id, agent_id);
            if let Some(guardrail) = &self.session.editing_guardrail {
                guardrail.release_all(&self.session.instance_id);
            }
            return None;
        }

        if self.session.editing.is_editing(session_id, agent_id) {
            Some(HookResult::Block(
                "call done_editing to get diagnostics before finishing".into(),
            ))
        } else {
            None
        }
    }

    /// Clear stale editing state on session start/resume.
    ///
    /// Returns the count of cleared entries, or `None` if nothing was cleared.
    /// Also releases any cross-session editing guardrail locks held by
    /// this session.
    fn handle_clear_editing(&self) -> Option<HookResult> {
        let count = self.session.editing.clear_all();
        if let Some(guardrail) = &self.session.editing_guardrail {
            guardrail.release_all(&self.session.instance_id);
        }

        if count > 0 {
            Some(HookResult::Cleared(count))
        } else {
            None
        }
    }

    /// Store the host CLI's session ID (idempotent — first write wins).
    fn store_client_session_id(&self, client_session_id: Option<&str>) {
        if let Some(client_sid) = client_session_id
            && let Ok(c) = self.conn.lock()
        {
            let _ = c.execute(
                "UPDATE sessions SET client_session_id = ?1 \
                 WHERE id = ?2 AND client_session_id IS NULL",
                rusqlite::params![client_sid, &*self.instance_id],
            );
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    use crate::config::Config;

    /// MCP-qualified `start_editing` name for test calls.
    const START_EDITING: &str = "mcp_catenary_start_editing";

    // ── Tool classification tests ───────────────────────────────────────

    #[test]
    fn test_is_edit_tool() {
        // Claude Code edit tools
        assert!(is_edit_tool("Edit"));
        assert!(is_edit_tool("Write"));
        assert!(is_edit_tool("NotebookEdit"));
        // Gemini CLI edit tools
        assert!(is_edit_tool("write_file"));
        assert!(is_edit_tool("replace"));
        // Non-edit tools
        assert!(!is_edit_tool("Read"));
        assert!(!is_edit_tool("Bash"));
        assert!(!is_edit_tool("grep"));
    }

    #[test]
    fn test_is_read_tool() {
        assert!(is_read_tool("Read"));
        assert!(is_read_tool("NotebookRead"));
        assert!(is_read_tool("read_file"));
        assert!(!is_read_tool("Edit"));
        assert!(!is_read_tool("Bash"));
    }

    #[test]
    fn test_is_catenary_tool() {
        // Bare name (direct MCP tool name)
        assert!(is_catenary_tool("grep", "grep"));
        assert!(is_catenary_tool("start_editing", "start_editing"));
        // Claude Code style: mcp__plugin_catenary_catenary__{suffix}
        assert!(is_catenary_tool(
            "mcp__plugin_catenary_catenary__grep",
            "grep"
        ));
        assert!(is_catenary_tool(
            "mcp__plugin_catenary_catenary__start_editing",
            "start_editing"
        ));
        // Gemini CLI style: mcp_catenary_{suffix}
        assert!(is_catenary_tool("mcp_catenary_grep", "grep"));
        assert!(is_catenary_tool(
            "mcp_catenary_start_editing",
            "start_editing"
        ));
        // Wrong suffix
        assert!(!is_catenary_tool("mcp_catenary_grep", "glob"));
        // Unrelated tool with matching substring — must not match
        assert!(!is_catenary_tool("grep_replace", "grep"));
        assert!(!is_catenary_tool("super_grep", "grep"));
    }

    #[test]
    fn test_is_allowed_during_editing() {
        // Bare Catenary tool names
        assert!(is_allowed_during_editing("start_editing"));
        assert!(is_allowed_during_editing("done_editing"));
        assert!(is_allowed_during_editing("grep"));
        assert!(is_allowed_during_editing("glob"));
        // Claude Code style: mcp__plugin_catenary_catenary__{suffix}
        assert!(is_allowed_during_editing(
            "mcp__plugin_catenary_catenary__start_editing"
        ));
        assert!(is_allowed_during_editing(
            "mcp__plugin_catenary_catenary__done_editing"
        ));
        assert!(is_allowed_during_editing(
            "mcp__plugin_catenary_catenary__grep"
        ));
        assert!(is_allowed_during_editing(
            "mcp__plugin_catenary_catenary__glob"
        ));
        // Gemini CLI style: mcp_catenary_{suffix}
        assert!(is_allowed_during_editing("mcp_catenary_start_editing"));
        assert!(is_allowed_during_editing("mcp_catenary_done_editing"));
        assert!(is_allowed_during_editing("mcp_catenary_grep"));
        assert!(is_allowed_during_editing("mcp_catenary_glob"));
        // ToolSearch (Claude Code deferred tool loader)
        assert!(is_allowed_during_editing("ToolSearch"));
        // Unrelated tools — must not match
        assert!(!is_allowed_during_editing("Edit"));
        assert!(!is_allowed_during_editing("Bash"));
        assert!(!is_allowed_during_editing("grep_replace"));
    }

    // ── Handler tests ───────────────────────────────────────────────────

    #[test]
    fn test_hook_enforce_editing_deny() {
        let router = test_router();
        // No editing state — Edit should be denied
        let result = router.handle_enforce_editing("Edit", None, None, None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny, got {result:?}");
        };
        assert!(reason.contains("start_editing"));
    }

    #[test]
    fn test_hook_enforce_editing_allow() {
        let router = test_router();
        // Enter editing mode through the hook handler
        let result = router.handle_enforce_editing(START_EDITING, None, None, None, "");
        assert!(result.is_none(), "start_editing should allow");
        assert!(
            router.session.editing.is_editing(None, ""),
            "should be in editing mode"
        );

        // Edit tool — should allow during editing mode
        let result = router.handle_enforce_editing("Edit", None, None, None, "");
        assert!(result.is_none(), "expected allow, got {result:?}");

        // Read tool — always allowed during editing
        let result = router.handle_enforce_editing("Read", None, None, None, "");
        assert!(result.is_none(), "expected allow for Read, got {result:?}");

        // Non-edit, non-read tool while editing — should deny
        let result = router.handle_enforce_editing("Bash", None, None, None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny for Bash, got {result:?}");
        };
        assert!(reason.contains("done_editing"));
    }

    #[test]
    fn test_hook_file_accumulation() {
        let (router, root) = test_router_with_root();
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        let main_rs = format!("{}/src/main.rs", root.display());

        // Edit tool accumulates file within root
        let result = router.handle_file_accumulation(&main_rs, None, "", Some("Edit"));
        assert!(result.is_none());

        // Read tool does not accumulate
        let lib_rs = format!("{}/src/lib.rs", root.display());
        let result = router.handle_file_accumulation(&lib_rs, None, "", Some("Read"));
        assert!(result.is_none());

        let files = router.session.editing.drain_files(None, "");
        assert_eq!(files, vec![PathBuf::from(&main_rs)]);
    }

    #[test]
    fn test_hook_require_release_block() {
        let router = test_router();
        // Enter editing mode through the hook handler
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        let result = router.handle_require_release(None, "", false);
        let Some(HookResult::Block(reason)) = result else {
            unreachable!("expected Block, got {result:?}");
        };
        assert!(reason.contains("done_editing"));
    }

    #[test]
    fn test_hook_require_release_allow() {
        let router = test_router();
        // No editing state — should allow
        let result = router.handle_require_release(None, "", false);
        assert!(result.is_none(), "expected allow, got {result:?}");
    }

    #[test]
    fn test_hook_require_release_retry() {
        let router = test_router();
        // Enter editing mode through the hook handler
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        // stop_hook_active = true → always allow regardless of state
        let result = router.handle_require_release(None, "", true);
        assert!(result.is_none(), "expected allow on retry, got {result:?}");

        // State should be cleared
        assert!(
            !router.session.editing.is_editing(None, ""),
            "editing state should be cleared after retry"
        );
    }

    #[test]
    fn test_hook_clear_editing() {
        let router = test_router();
        // Enter editing mode for two agents through the hook handler
        router.handle_enforce_editing(START_EDITING, None, None, None, "");
        router.handle_enforce_editing(START_EDITING, None, None, None, "agent-b");

        let result = router.handle_clear_editing();
        assert_eq!(result, Some(HookResult::Cleared(2)));

        // Second call should return None (nothing to clear)
        let result = router.handle_clear_editing();
        assert!(
            result.is_none(),
            "expected None after clear, got {result:?}"
        );
    }

    // ── PreToolStartEditing dispatch tests ─────────────────────────────

    #[test]
    fn dispatch_start_editing_cli_enters_editing() {
        let router = test_router();
        assert!(!router.session.editing.is_editing(None, ""));

        let result = router.dispatch(
            crate::hook::HookRequest::PreToolStartEditing {
                agent_id: String::new(),
                session_id: None,
            },
            0,
        );
        assert!(result.result.is_none(), "start_editing should allow");
        assert!(
            router.session.editing.is_editing(None, ""),
            "should be in editing mode after dispatch"
        );
    }

    #[test]
    fn dispatch_start_editing_cli_with_agent_id() {
        let router = test_router();
        let result = router.dispatch(
            crate::hook::HookRequest::PreToolStartEditing {
                agent_id: "sub-agent".to_string(),
                session_id: None,
            },
            0,
        );
        assert!(result.result.is_none());
        assert!(router.session.editing.is_editing(None, "sub-agent"));
        assert!(!router.session.editing.is_editing(None, ""));
    }

    #[test]
    fn dispatch_start_editing_cli_then_edit_allowed() {
        let (router, root) = test_router_with_root();
        // Enter editing via the CLI path.
        router.dispatch(
            crate::hook::HookRequest::PreToolStartEditing {
                agent_id: String::new(),
                session_id: None,
            },
            0,
        );

        // Edit tool should now be allowed.
        let in_root = format!("{}/src/main.rs", root.display());
        let result = router.handle_enforce_editing("Edit", Some(&in_root), None, None, "");
        assert!(
            result.is_none(),
            "Edit should be allowed after start_editing CLI, got {result:?}"
        );
    }

    // ── Scope boundary tests ──────────────────────────────────────────

    #[test]
    fn test_enforce_editing_skip_gate_for_out_of_root_file() {
        let router = test_router();
        // Edit on a file outside workspace roots while not editing →
        // should allow (no diagnostics will come for out-of-root files).
        let result =
            router.handle_enforce_editing("Edit", Some("/outside/some/file.rs"), None, None, "");
        assert!(
            result.is_none(),
            "out-of-root edit should be allowed without start_editing, got {result:?}"
        );
    }

    #[test]
    fn test_enforce_editing_still_denies_in_root_file() {
        let (router, root) = test_router_with_root();
        // Edit on a file inside workspace roots while not editing → deny.
        let in_root = format!("{}/src/main.rs", root.display());
        let result = router.handle_enforce_editing("Edit", Some(&in_root), None, None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny for in-root edit, got {result:?}");
        };
        assert!(reason.contains("start_editing"));
    }

    #[test]
    fn test_enforce_editing_no_file_path_still_denies() {
        let router = test_router();
        // Edit with no file path (e.g., host didn't supply it) → deny.
        let result = router.handle_enforce_editing("Edit", None, None, None, "");
        let Some(HookResult::Deny(_)) = result else {
            unreachable!("expected Deny when file_path is None, got {result:?}");
        };
    }

    #[test]
    fn test_file_accumulation_skips_out_of_root() {
        let router = test_router();
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        // File outside workspace roots — should not be accumulated.
        router.handle_file_accumulation("/outside/some/file.rs", None, "", Some("Edit"));
        let files = router.session.editing.drain_files(None, "");
        assert!(
            files.is_empty(),
            "out-of-root file should not be accumulated"
        );
    }

    #[test]
    fn test_file_accumulation_keeps_in_root() {
        let (router, root) = test_router_with_root();
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        let in_root = format!("{}/src/main.rs", root.display());
        router.handle_file_accumulation(&in_root, None, "", Some("Edit"));
        let files = router.session.editing.drain_files(None, "");
        assert_eq!(files.len(), 1, "in-root file should be accumulated");
    }

    // ── Single-file cache scope boundary tests ─────────────────────────

    /// Fake language ID matching the manager tests. Files with extension
    /// `.yX4Za` resolve to this via the raw-extension fallback in
    /// `language_id()`.
    const SF_LANG: &str = "yX4Za";
    const SF_SERVER: &str = "mockls-sf";

    /// Build a config with a single language+server for single-file
    /// cache tests. No real LSP binary needed — these tests only check
    /// cache-driven routing in the hook layer.
    fn sf_test_config() -> Config {
        use crate::config::{LanguageConfig, ServerBinding, ServerDef};

        let mut config = Config::default();
        config.server.insert(
            SF_SERVER.to_string(),
            ServerDef {
                command: "mockls".to_string(),
                args: vec![SF_LANG.to_string()],
                single_file: true,
                ..ServerDef::default()
            },
        );
        config.language.insert(
            SF_LANG.to_string(),
            LanguageConfig {
                servers: Some(vec![ServerBinding::new(SF_SERVER.to_string())]),
                ..LanguageConfig::default()
            },
        );
        config
    }

    /// Create a `HookRouter` with `single_file = true` in config.
    /// When `failed` is true, injects a negative-cache entry so the
    /// server appears to have rejected null-workspace initialization.
    fn test_router_with_sf_config(failed: bool) -> TestHookRouter {
        let (dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('test-session', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let config = sf_test_config();
        let logging = crate::logging::LoggingServer::new();
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let instance_id: Arc<str> = "test-session".into();
        let notification_router = Arc::new(
            crate::logging::notification_router::NotificationRouter::new(
                crate::logging::Severity::Warn,
            ),
        );
        notification_router.register_session(&instance_id);
        let session = Arc::new(Session::new(
            config,
            vec![],
            logging,
            conn.clone(),
            instance_id.clone(),
            handle,
            notification_router,
        ));

        if failed {
            session
                .client_manager
                .single_file_failures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert((SF_LANG.to_string(), SF_SERVER.to_string()));
        }

        let router = HookRouter::new(session, conn, instance_id, "test".to_string());

        TestHookRouter {
            _dir: dir,
            _runtime: runtime,
            router,
        }
    }

    #[test]
    fn test_enforce_editing_gates_out_of_root_with_single_file_config() {
        // single_file = true, no failure → server expected to work → gate.
        let router = test_router_with_sf_config(false);
        let path = format!("/outside/file.{SF_LANG}");
        let result = router.handle_enforce_editing("Edit", Some(&path), None, None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny for single_file out-of-root edit, got {result:?}");
        };
        assert!(reason.contains("start_editing"));
    }

    #[test]
    fn test_enforce_editing_skips_out_of_root_with_runtime_failure() {
        // single_file = true but server rejected at runtime → skip gate.
        let router = test_router_with_sf_config(true);
        let path = format!("/outside/file.{SF_LANG}");
        let result = router.handle_enforce_editing("Edit", Some(&path), None, None, "");
        assert!(
            result.is_none(),
            "runtime-failed out-of-root edit should be allowed, got {result:?}"
        );
    }

    #[test]
    fn test_enforce_editing_skips_out_of_root_without_single_file_config() {
        // No single_file config at all → skip gate.
        let router = test_router();
        let result =
            router.handle_enforce_editing("Edit", Some("/outside/some/file.rs"), None, None, "");
        assert!(
            result.is_none(),
            "out-of-root edit without single_file config should be allowed, got {result:?}"
        );
    }

    #[test]
    fn test_file_accumulation_includes_out_of_root_with_single_file_config() {
        // single_file = true, no failure → file should be accumulated.
        let router = test_router_with_sf_config(false);
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        let path = format!("/outside/file.{SF_LANG}");
        router.handle_file_accumulation(&path, None, "", Some("Edit"));
        let files = router.session.editing.drain_files(None, "");
        assert_eq!(
            files.len(),
            1,
            "single_file out-of-root file should be accumulated"
        );
    }

    #[test]
    fn test_file_accumulation_skips_out_of_root_with_runtime_failure() {
        // single_file = true but runtime failure → file should NOT be accumulated.
        let router = test_router_with_sf_config(true);
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        let path = format!("/outside/file.{SF_LANG}");
        router.handle_file_accumulation(&path, None, "", Some("Edit"));
        let files = router.session.editing.drain_files(None, "");
        assert!(
            files.is_empty(),
            "runtime-failed out-of-root file should not be accumulated"
        );
    }

    // ── Filesystem Bash allowlist tests ──────────────────────────────────

    #[test]
    fn test_is_bash_tool() {
        assert!(is_bash_tool("Bash"));
        assert!(is_bash_tool("run_shell_command"));
        assert!(!is_bash_tool("Edit"));
        assert!(!is_bash_tool("Read"));
        assert!(!is_bash_tool("bash")); // case-sensitive
    }

    #[test]
    fn test_is_filesystem_only_bash() {
        // Single filesystem commands
        assert!(is_filesystem_only_bash("rm -rf target/"));
        assert!(is_filesystem_only_bash("cp src/old.rs src/new.rs"));
        assert!(is_filesystem_only_bash("mv foo.rs bar.rs"));
        assert!(is_filesystem_only_bash("mkdir -p src/new_module"));
        assert!(is_filesystem_only_bash("rmdir empty_dir"));
        assert!(is_filesystem_only_bash("touch src/mod.rs"));
        assert!(is_filesystem_only_bash("chmod +x script.sh"));

        // Chained filesystem commands
        assert!(is_filesystem_only_bash(
            "rm src/old.rs && mkdir -p src/new/"
        ));
        assert!(is_filesystem_only_bash("cp a.rs b.rs; mv c.rs d.rs"));

        // Full paths stripped to bare names
        assert!(is_filesystem_only_bash("/bin/rm foo.rs"));
        assert!(is_filesystem_only_bash("/usr/bin/cp a b"));

        // With env var prefixes
        assert!(is_filesystem_only_bash("LANG=C rm foo.rs"));

        // Non-filesystem commands — must deny
        assert!(!is_filesystem_only_bash("cargo build"));
        assert!(!is_filesystem_only_bash("cat src/main.rs"));
        assert!(!is_filesystem_only_bash("rm foo.rs && cargo test"));

        // Mixed: one filesystem + one non-filesystem
        assert!(!is_filesystem_only_bash("rm foo.rs && grep bar baz.rs"));

        // Subshell with non-filesystem command
        assert!(!is_filesystem_only_bash("rm $(cat files.txt)"));

        // Empty command
        assert!(!is_filesystem_only_bash(""));
    }

    #[test]
    fn test_enforce_editing_allows_filesystem_bash() {
        let router = test_router();
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        // Filesystem-only Bash — should allow during editing
        let result = router.handle_enforce_editing("Bash", None, Some("rm -rf target/"), None, "");
        assert!(
            result.is_none(),
            "filesystem-only Bash should be allowed during editing, got {result:?}"
        );

        // Gemini CLI shell tool with filesystem command
        let result = router.handle_enforce_editing(
            "run_shell_command",
            None,
            Some("mkdir -p src/new_module"),
            None,
            "",
        );
        assert!(
            result.is_none(),
            "filesystem-only run_shell_command should be allowed, got {result:?}"
        );
    }

    #[test]
    fn test_enforce_editing_denies_non_filesystem_bash() {
        let router = test_router();
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        // Non-filesystem Bash — should deny during editing
        let result = router.handle_enforce_editing("Bash", None, Some("cargo build"), None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny for non-filesystem Bash, got {result:?}");
        };
        assert!(reason.contains("done_editing"));
    }

    #[test]
    fn test_enforce_editing_denies_bash_without_command() {
        let router = test_router();
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        // Bash without command string — cannot verify, must deny
        let result = router.handle_enforce_editing("Bash", None, None, None, "");
        let Some(HookResult::Deny(_)) = result else {
            unreachable!("expected Deny for Bash without command, got {result:?}");
        };
    }

    // ── Test helpers ────────────────────────────────────────────────────

    /// Open an isolated test database in a tempdir.
    fn test_db() -> (tempfile::TempDir, std::path::PathBuf, rusqlite::Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("catenary").join("catenary.db");
        let conn = crate::db::open_and_migrate_at(&path).expect("open test DB");
        (dir, path, conn)
    }

    /// Create a `HookRouter` with a test database for handler unit tests.
    ///
    /// Uses minimal dependencies (no live LSP servers). Editing state is
    /// managed in-memory via [`super::super::editing_manager::EditingManager`]
    /// on the `Session`.
    fn test_router() -> TestHookRouter {
        let (dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        // Insert a session for FK constraints.
        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('test-session', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let config = Config::default();
        let logging = crate::logging::LoggingServer::new();

        // Session requires a tokio runtime handle for async dispatch.
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let instance_id: Arc<str> = "test-session".into();
        let notification_router = Arc::new(
            crate::logging::notification_router::NotificationRouter::new(
                crate::logging::Severity::Warn,
            ),
        );
        notification_router.register_session(&instance_id);
        let session = Arc::new(Session::new(
            config,
            vec![],
            logging,
            conn.clone(),
            instance_id.clone(),
            handle,
            notification_router,
        ));
        let router = HookRouter::new(session, conn, instance_id, "test".to_string());

        TestHookRouter {
            _dir: dir,
            _runtime: runtime,
            router,
        }
    }

    /// Create a `HookRouter` with a workspace root for scope boundary tests.
    fn test_router_with_root() -> (TestHookRouter, PathBuf) {
        let (dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('test-session', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let config = Config::default();
        let logging = crate::logging::LoggingServer::new();

        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");

        let instance_id: Arc<str> = "test-session".into();
        let notification_router = Arc::new(
            crate::logging::notification_router::NotificationRouter::new(
                crate::logging::Severity::Warn,
            ),
        );
        notification_router.register_session(&instance_id);
        let session = Arc::new(Session::new(
            config,
            vec![root.clone()],
            logging,
            conn.clone(),
            instance_id.clone(),
            handle,
            notification_router,
        ));

        let router = HookRouter::new(session, conn, instance_id, "test".to_string());

        (
            TestHookRouter {
                _dir: dir,
                _runtime: runtime,
                router,
            },
            root,
        )
    }

    /// Wrapper that keeps the tempdir and runtime alive for the lifetime of the router.
    struct TestHookRouter {
        _dir: tempfile::TempDir,
        _runtime: tokio::runtime::Runtime,
        router: HookRouter,
    }

    impl std::ops::Deref for TestHookRouter {
        type Target = HookRouter;
        fn deref(&self) -> &Self::Target {
            &self.router
        }
    }

    // ── Dispatch-level drain tests ─────────────────────────────────────

    #[test]
    fn dispatch_session_start_drains_notifications() {
        let router = test_router();
        // Populate the notification queue.
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );
        assert_eq!(
            router.session.notification_router.queue_len("test-session"),
            1
        );

        let result = router.dispatch(
            crate::hook::HookRequest::SessionStart { session_id: None },
            0,
        );
        assert!(
            result.system_message.is_some(),
            "session start should drain notifications"
        );
        assert!(router.session.notification_router.queue_len("test-session") == 0);
    }

    #[test]
    fn dispatch_stop_allow_drains_notifications() {
        let router = test_router();
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );

        // Not editing → allow → should drain.
        let result = router.dispatch(
            crate::hook::HookRequest::PostAgent {
                agent_id: String::new(),
                session_id: None,
                stop_hook_active: false,
            },
            0,
        );
        assert!(result.result.is_none(), "should allow");
        assert!(
            result.system_message.is_some(),
            "allow should drain notifications"
        );
        assert!(router.session.notification_router.queue_len("test-session") == 0);
    }

    #[test]
    fn dispatch_stop_block_preserves_notifications() {
        let router = test_router();
        // Enter editing mode so stop blocks.
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );

        let result = router.dispatch(
            crate::hook::HookRequest::PostAgent {
                agent_id: String::new(),
                session_id: None,
                stop_hook_active: false,
            },
            0,
        );
        assert!(
            matches!(result.result, Some(HookResult::Block(_))),
            "should block"
        );
        assert!(result.system_message.is_none(), "block should not drain");
        assert_eq!(
            router.session.notification_router.queue_len("test-session"),
            1,
            "queue should be preserved"
        );
    }

    #[test]
    fn dispatch_pre_tool_does_not_drain() {
        let router = test_router();
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );

        let result = router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "Read".to_string(),
                file_path: None,
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: None,
            },
            0,
        );
        assert!(result.system_message.is_none(), "pre-tool should not drain");
        assert_eq!(
            router.session.notification_router.queue_len("test-session"),
            1
        );
    }

    #[test]
    fn dispatch_stop_block_then_allow_drains_accumulated() {
        let router = test_router();
        // Enter editing mode so stop blocks.
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        // Enqueue a notification before the first stop.
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );

        // First stop: block (editing active) — queue preserved.
        let result = router.dispatch(
            crate::hook::HookRequest::PostAgent {
                agent_id: String::new(),
                session_id: None,
                stop_hook_active: false,
            },
            0,
        );
        assert!(matches!(result.result, Some(HookResult::Block(_))));
        assert!(result.system_message.is_none());
        assert_eq!(
            router.session.notification_router.queue_len("test-session"),
            1
        );

        // Enqueue another notification between block and retry.
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("config error", "pylsp"),
        );
        assert_eq!(
            router.session.notification_router.queue_len("test-session"),
            2
        );

        // Second stop: retry (stop_hook_active) — force-clears editing, allows, drains.
        let result = router.dispatch(
            crate::hook::HookRequest::PostAgent {
                agent_id: String::new(),
                session_id: None,
                stop_hook_active: true,
            },
            0,
        );
        assert!(result.result.is_none(), "retry should allow");
        let msg = result
            .system_message
            .expect("retry-allow should drain accumulated notifications");
        assert!(
            msg.contains("server offline"),
            "drain should include first-cycle notification"
        );
        assert!(
            msg.contains("config error"),
            "drain should include second-cycle notification"
        );
        assert!(router.session.notification_router.queue_len("test-session") == 0);
    }

    #[test]
    fn dispatch_stop_dedup_persists_across_blocked_cycle() {
        let router = test_router();
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        // Enqueue a notification.
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );

        // Block — queue preserved.
        let result = router.dispatch(
            crate::hook::HookRequest::PostAgent {
                agent_id: String::new(),
                session_id: None,
                stop_hook_active: false,
            },
            0,
        );
        assert!(matches!(result.result, Some(HookResult::Block(_))));

        // Same notification again — dedup should reject.
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );
        assert_eq!(
            router.session.notification_router.queue_len("test-session"),
            1,
            "dedup should reject duplicate across blocked cycle"
        );

        // Retry-allow: drain should contain exactly one notification.
        let result = router.dispatch(
            crate::hook::HookRequest::PostAgent {
                agent_id: String::new(),
                session_id: None,
                stop_hook_active: true,
            },
            0,
        );
        let msg = result.system_message.expect("should drain");
        // Background header + 1 notification = 2 lines.
        assert_eq!(
            msg.lines().count(),
            2,
            "expected header + 1 notification, got: {msg}"
        );
    }

    /// Shorthand for constructing a notification-level `LogEvent`.
    ///
    /// Includes `session_id = "test-session"` so the `NotificationRouter`
    /// routes the event to the test session's queue.
    fn make_notify_event(message: &str, server: &str) -> crate::logging::LogEvent<'static> {
        crate::logging::LogEvent {
            severity: crate::logging::Severity::Warn,
            target: "test",
            message: message.to_string(),
            kind: None,
            method: None,
            server: Some(server.to_string()),
            client: None,
            request_id: None,
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            session_id: Some("test-session".to_string()),
            fields: serde_json::Map::new(),
        }
    }

    #[test]
    fn dispatch_post_tool_does_not_drain() {
        let router = test_router();
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );

        let result = router.dispatch(
            crate::hook::HookRequest::PostTool {
                file: "/tmp/test.rs".to_string(),
                tool: Some("Edit".to_string()),
                agent_id: String::new(),
                session_id: None,
            },
            0,
        );
        assert!(
            result.system_message.is_none(),
            "post-tool should not drain"
        );
        assert_eq!(
            router.session.notification_router.queue_len("test-session"),
            1
        );
    }

    // ── Turn counter tests ────────────────────────────────────────────

    #[test]
    fn turn_counter_increments_on_dispatch() {
        let router = test_router();
        assert_eq!(router.turn(), 0);

        router.dispatch(
            crate::hook::HookRequest::PreAgent {
                transcript_path: None,
                session_id: None,
            },
            0,
        );
        assert_eq!(router.turn(), 1);

        router.dispatch(
            crate::hook::HookRequest::PreAgent {
                transcript_path: None,
                session_id: None,
            },
            0,
        );
        assert_eq!(router.turn(), 2);
    }

    #[test]
    fn pre_agent_sets_roots_refresh_flag() {
        let router = test_router();
        assert!(
            !router
                .session
                .roots_refresh_requested
                .load(Ordering::Acquire),
            "flag should start false"
        );

        router.dispatch(
            crate::hook::HookRequest::PreAgent {
                transcript_path: None,
                session_id: None,
            },
            0,
        );
        assert!(
            router
                .session
                .roots_refresh_requested
                .load(Ordering::Acquire),
            "flag should be set after PreAgent"
        );
    }

    // ── Command check + debounce tests ────────────────────────────

    /// Create a test router with an active command allowlist.
    ///
    /// Allows only `git` — any other command (e.g., `cargo`) is denied.
    fn test_router_with_commands() -> TestHookRouter {
        let (dir, _path, conn) = test_db();
        let conn = Arc::new(std::sync::Mutex::new(conn));

        conn.lock()
            .expect("lock")
            .execute(
                "INSERT INTO sessions (id, pid, display_name, started_at) \
                 VALUES ('test-session', 1, 'test', '2026-01-01T00:00:00Z')",
                [],
            )
            .expect("insert session");

        let config = Config {
            resolved_commands: Some(crate::config::ResolvedCommands {
                allow: std::collections::HashSet::from(["git".into()]),
                ..crate::config::ResolvedCommands::default()
            }),
            ..Config::default()
        };
        let logging = crate::logging::LoggingServer::new();
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();
        let instance_id: Arc<str> = "test-session".into();
        let notification_router = Arc::new(
            crate::logging::notification_router::NotificationRouter::new(
                crate::logging::Severity::Warn,
            ),
        );
        notification_router.register_session(&instance_id);
        let session = Arc::new(Session::new(
            config,
            vec![],
            logging,
            conn.clone(),
            instance_id.clone(),
            handle,
            notification_router,
        ));
        let router = HookRouter::new(session, conn, instance_id, "test".to_string());
        TestHookRouter {
            _dir: dir,
            _runtime: runtime,
            router,
        }
    }

    fn dispatch_check_denied(router: &HookRouter) -> DispatchResult {
        router.dispatch(
            crate::hook::HookRequest::CheckCommand {
                command: "cargo test".to_string(),
                cwd: None,
                session_id: None,
                format: None,
            },
            0,
        )
    }

    fn dispatch_check_allowed(router: &HookRouter) -> DispatchResult {
        router.dispatch(
            crate::hook::HookRequest::CheckCommand {
                command: "git status".to_string(),
                cwd: None,
                session_id: None,
                format: None,
            },
            0,
        )
    }

    #[test]
    fn check_command_allowed_returns_none() {
        let router = test_router_with_commands();
        let result = dispatch_check_allowed(&router);
        assert!(
            result.result.is_none(),
            "allowed command should return no result"
        );
    }

    #[test]
    fn check_command_denied_first_returns_full() {
        let router = test_router_with_commands();
        let result = dispatch_check_denied(&router);
        let Some(HookResult::Deny(msg)) = result.result else {
            unreachable!("expected Deny, got {:?}", result.result);
        };
        assert!(
            msg.contains("cargo"),
            "full dump should name denied command"
        );
        assert!(
            msg.contains("Allowed:"),
            "full dump should list allowed commands"
        );
    }

    #[test]
    fn check_command_denied_second_returns_short() {
        let router = test_router_with_commands();
        // First denial → full (stores current turn).
        dispatch_check_denied(&router);

        // Second denial in same turn → short.
        let result = dispatch_check_denied(&router);
        let Some(HookResult::Deny(msg)) = result.result else {
            unreachable!("expected Deny, got {:?}", result.result);
        };
        assert!(
            msg.contains("see earlier message"),
            "subsequent denial should be short"
        );
    }

    #[test]
    fn check_command_new_turn_resets_to_full() {
        let router = test_router_with_commands();
        dispatch_check_denied(&router);
        dispatch_check_denied(&router); // short

        // Advance turn, next denial → full again.
        router.dispatch(
            crate::hook::HookRequest::PreAgent {
                transcript_path: None,
                session_id: None,
            },
            0,
        );
        let result = dispatch_check_denied(&router);
        let Some(HookResult::Deny(msg)) = result.result else {
            unreachable!("expected Deny, got {:?}", result.result);
        };
        assert!(
            msg.contains("Allowed:"),
            "new turn should reset to full dump"
        );
    }

    #[test]
    fn check_command_config_version_forces_full() {
        let router = test_router_with_commands();
        dispatch_check_denied(&router);
        dispatch_check_denied(&router); // short

        // Bump config version → full again.
        router.bump_config_version();
        let result = dispatch_check_denied(&router);
        let Some(HookResult::Deny(msg)) = result.result else {
            unreachable!("expected Deny, got {:?}", result.result);
        };
        assert!(
            msg.contains("Allowed:"),
            "config version change should force full dump"
        );
    }

    #[test]
    fn check_command_no_config_returns_none() {
        // Default router has no [commands] → check-command returns allow.
        let router = test_router();
        let result = dispatch_check_denied(&router);
        assert!(
            result.result.is_none(),
            "no commands config should return no result"
        );
    }

    // ── PreToolUse file tracking tests ──────────────────────────────

    #[test]
    fn dispatch_pre_tool_edit_accumulates_file() {
        let (router, root) = test_router_with_root();
        // Enter editing mode.
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        let file = format!("{}/src/main.rs", root.display());
        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "Edit".to_string(),
                file_path: Some(file.clone()),
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: None,
            },
            0,
        );

        let files = router.session.editing.drain_files(None, "");
        assert_eq!(
            files,
            vec![PathBuf::from(&file)],
            "PreTool for edit tool should accumulate file"
        );
    }

    #[test]
    fn dispatch_pre_tool_denied_does_not_accumulate() {
        let (router, root) = test_router_with_root();
        // NOT in editing mode — Edit will be denied.
        let file = format!("{}/src/main.rs", root.display());
        let result = router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "Edit".to_string(),
                file_path: Some(file),
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: None,
            },
            0,
        );

        assert!(
            matches!(result.result, Some(HookResult::Deny(_))),
            "Edit outside editing mode should be denied"
        );
        // Enter editing to drain (drain_files requires editing state).
        router.handle_enforce_editing(START_EDITING, None, None, None, "");
        let files = router.session.editing.drain_files(None, "");
        assert!(files.is_empty(), "denied edit should not accumulate file");
    }

    #[test]
    fn dispatch_pre_tool_non_edit_does_not_accumulate() {
        let (router, root) = test_router_with_root();
        // Enter editing mode.
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        let file = format!("{}/src/main.rs", root.display());
        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "mcp_catenary_grep".to_string(),
                file_path: Some(file),
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: None,
            },
            0,
        );

        let files = router.session.editing.drain_files(None, "");
        assert!(files.is_empty(), "non-edit tool should not accumulate file");
    }

    // ── CWD stash tests ─────────────────────────────────────────────

    #[test]
    fn dispatch_pre_tool_grep_stashes_cwd() {
        let router = test_router();
        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "mcp__plugin_catenary_catenary__grep".to_string(),
                file_path: None,
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: Some("/home/user/project".to_string()),
            },
            0,
        );
        let stashed = router.session.cwd_stash.take();
        assert_eq!(stashed, Some(PathBuf::from("/home/user/project")));
    }

    #[test]
    fn dispatch_pre_tool_glob_stashes_cwd() {
        let router = test_router();
        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "mcp_catenary_glob".to_string(),
                file_path: None,
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: Some("/workspace".to_string()),
            },
            0,
        );
        assert_eq!(
            router.session.cwd_stash.take(),
            Some(PathBuf::from("/workspace"))
        );
    }

    #[test]
    fn dispatch_pre_tool_non_catenary_does_not_stash() {
        let router = test_router();
        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "Read".to_string(),
                file_path: None,
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: Some("/home/user".to_string()),
            },
            0,
        );
        assert!(
            router.session.cwd_stash.take().is_none(),
            "non-Catenary tools should not stash cwd"
        );
    }

    #[test]
    fn dispatch_pre_tool_denied_does_not_stash() {
        let router = test_router();
        // Enter editing mode so non-allowed tools are denied.
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "Bash".to_string(),
                file_path: None,
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: Some("/should/not/stash".to_string()),
            },
            0,
        );
        assert!(
            router.session.cwd_stash.take().is_none(),
            "denied tools should not stash cwd"
        );
    }

    #[test]
    fn dispatch_pre_tool_no_cwd_does_not_stash() {
        let router = test_router();
        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "mcp_catenary_grep".to_string(),
                file_path: None,
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: None,
            },
            0,
        );
        assert!(
            router.session.cwd_stash.take().is_none(),
            "missing cwd should not stash"
        );
    }

    // ── Scope ID stash tests ───────────────────────────────────────────

    #[test]
    fn dispatch_pre_tool_stashes_scope_id() {
        let router = test_router();
        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "mcp_catenary_grep".to_string(),
                file_path: None,
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: None,
            },
            99,
        );
        assert!(
            router.session.scope_id_stash.peek().is_some(),
            "pre-tool should stash scope UUID as scope parent"
        );
    }

    #[test]
    fn dispatch_pre_tool_scope_id_survives_take_cycle() {
        let router = test_router();
        // Pre-tool stashes.
        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "mcp_catenary_grep".to_string(),
                file_path: None,
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: None,
            },
            10,
        );
        // Peek (MCP handler) does not consume.
        let peeked = router.session.scope_id_stash.peek();
        assert!(peeked.is_some(), "peek should return the stashed UUID");
        // Take (post-tool hook) clears.
        let taken = router.session.scope_id_stash.take();
        assert_eq!(taken, peeked, "take should return the same UUID as peek");
        assert!(router.session.scope_id_stash.peek().is_none());
    }

    #[test]
    fn dispatch_pre_tool_denied_does_not_stash_scope_id() {
        let router = test_router();
        // Enter editing mode so non-allowed tools are denied.
        router.handle_enforce_editing(START_EDITING, None, None, None, "");

        router.dispatch(
            crate::hook::HookRequest::PreTool {
                tool_name: "Bash".to_string(),
                file_path: None,
                command: None,
                agent_id: String::new(),
                session_id: None,
                cwd: None,
            },
            42,
        );
        assert!(
            router.session.scope_id_stash.peek().is_none(),
            "denied tools should not stash scope_id"
        );
    }

    // ── Transcript root sync tests ────────────────────────────────────

    /// Write a transcript JSONL file with the given lines.
    fn write_transcript(dir: &std::path::Path, lines: &[&str]) -> PathBuf {
        let path = dir.join("transcript.jsonl");
        std::fs::write(&path, lines.join("\n")).expect("write transcript");
        path
    }

    /// Create a real directory so `canonicalize` succeeds.
    fn make_dir(base: &std::path::Path, name: &str) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).expect("create dir");
        dir.canonicalize().expect("canonicalize")
    }

    #[test]
    fn scan_transcript_finds_add_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = make_dir(dir.path(), "project");
        let line = format!(
            r#"{{"message":"Added \u001b[1m{}\u001b[22m as a working directory"}}"#,
            root.display()
        );
        let transcript = write_transcript(dir.path(), &[&line]);

        let router = test_router();
        *router.session.transcript_path.lock().expect("lock") = Some(transcript);

        let roots = scan_transcript(&router.session);
        assert_eq!(roots, vec![root]);
    }

    #[test]
    #[allow(
        clippy::similar_names,
        reason = "root1/root2 naming is clear in context"
    )]
    fn scan_transcript_incremental() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root1 = make_dir(dir.path(), "project1");
        let line1 = format!(
            r#"{{"message":"Added \u001b[1m{}\u001b[22m as a working directory"}}"#,
            root1.display()
        );
        let transcript = write_transcript(dir.path(), &[&line1]);

        let router = test_router();
        *router.session.transcript_path.lock().expect("lock") = Some(transcript.clone());

        let roots = scan_transcript(&router.session);
        assert_eq!(roots, vec![root1]);

        // Second scan with same content → empty (offset advanced).
        let roots = scan_transcript(&router.session);
        assert!(roots.is_empty(), "second scan should find nothing new");

        // Append a new line and scan again.
        let root2 = make_dir(dir.path(), "project2");
        let line2 = format!(
            r#"{{"message":"Added \u001b[1m{}\u001b[22m as a working directory"}}"#,
            root2.display()
        );
        let mut content = std::fs::read_to_string(&transcript).expect("read");
        content.push('\n');
        content.push_str(&line2);
        std::fs::write(&transcript, content).expect("append");

        let roots = scan_transcript(&router.session);
        assert_eq!(roots, vec![root2]);
    }

    #[test]
    #[allow(
        clippy::similar_names,
        reason = "root1/root2 naming is clear in context"
    )]
    fn scan_transcript_multiple_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root1 = make_dir(dir.path(), "a");
        let root2 = make_dir(dir.path(), "b");
        let line1 = format!(
            r#"{{"message":"Added \u001b[1m{}\u001b[22m as a working directory"}}"#,
            root1.display()
        );
        let line2 = format!(
            r#"{{"message":"Added \u001b[1m{}\u001b[22m as a working directory"}}"#,
            root2.display()
        );
        let transcript = write_transcript(dir.path(), &[&line1, &line2]);

        let router = test_router();
        *router.session.transcript_path.lock().expect("lock") = Some(transcript);

        let roots = scan_transcript(&router.session);
        assert_eq!(roots.len(), 2);
        assert!(roots.contains(&root1));
        assert!(roots.contains(&root2));
    }

    #[test]
    fn scan_transcript_deduplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = make_dir(dir.path(), "dup");
        let line = format!(
            r#"{{"message":"Added \u001b[1m{}\u001b[22m as a working directory"}}"#,
            root.display()
        );
        let transcript = write_transcript(dir.path(), &[&line, &line]);

        let router = test_router();
        *router.session.transcript_path.lock().expect("lock") = Some(transcript);

        let roots = scan_transcript(&router.session);
        assert_eq!(roots, vec![root]);
    }

    #[test]
    fn scan_transcript_skips_non_matching() {
        let dir = tempfile::tempdir().expect("tempdir");
        let transcript = write_transcript(
            dir.path(),
            &[
                r#"{"message":"some other line"}"#,
                r#"{"message":"tool call completed"}"#,
            ],
        );

        let router = test_router();
        *router.session.transcript_path.lock().expect("lock") = Some(transcript);

        let roots = scan_transcript(&router.session);
        assert!(roots.is_empty());
    }

    #[test]
    fn scan_transcript_missing_file() {
        let router = test_router();
        *router.session.transcript_path.lock().expect("lock") =
            Some(PathBuf::from("/nonexistent/transcript.jsonl"));

        let roots = scan_transcript(&router.session);
        assert!(roots.is_empty());
    }

    #[test]
    fn scan_transcript_no_path_stashed() {
        let router = test_router();
        let roots = scan_transcript(&router.session);
        assert!(roots.is_empty());
    }

    #[test]
    fn dispatch_pre_agent_stashes_transcript_path() {
        let router = test_router();
        assert!(
            router
                .session
                .transcript_path
                .lock()
                .expect("lock")
                .is_none()
        );

        router.dispatch(
            crate::hook::HookRequest::PreAgent {
                transcript_path: Some("/tmp/transcript.jsonl".to_string()),
                session_id: None,
            },
            0,
        );
        assert_eq!(
            router
                .session
                .transcript_path
                .lock()
                .expect("lock")
                .as_deref(),
            Some(std::path::Path::new("/tmp/transcript.jsonl")),
        );
    }

    #[test]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "lock guard dropped at end of test"
    )]
    fn dispatch_pre_agent_returns_transcript_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = make_dir(dir.path(), "new_root");
        let line = format!(
            r#"{{"message":"Added \u001b[1m{}\u001b[22m as a working directory"}}"#,
            root.display()
        );
        let transcript = write_transcript(dir.path(), &[&line]);

        let router = test_router();
        let result = router.dispatch(
            crate::hook::HookRequest::PreAgent {
                transcript_path: Some(transcript.to_string_lossy().to_string()),
                session_id: None,
            },
            0,
        );
        assert_eq!(result.add_roots, vec![root.clone()]);

        // Verify the roots are stored in transcript_roots.
        let stored = router.session.transcript_roots.lock().expect("lock");
        assert_eq!(stored.as_slice(), &[root]);
    }

    #[test]
    #[allow(
        clippy::similar_names,
        reason = "root1/root2 naming is clear in context"
    )]
    #[allow(
        clippy::significant_drop_tightening,
        reason = "lock guard dropped at end of test"
    )]
    fn transcript_roots_accumulate() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root1 = make_dir(dir.path(), "first");
        let line1 = format!(
            r#"{{"message":"Added \u001b[1m{}\u001b[22m as a working directory"}}"#,
            root1.display()
        );
        let transcript = write_transcript(dir.path(), &[&line1]);

        let router = test_router();

        // First dispatch: discovers root1.
        router.dispatch(
            crate::hook::HookRequest::PreAgent {
                transcript_path: Some(transcript.to_string_lossy().to_string()),
                session_id: None,
            },
            0,
        );

        // Append root2 and dispatch again.
        let root2 = make_dir(dir.path(), "second");
        let line2 = format!(
            r#"{{"message":"Added \u001b[1m{}\u001b[22m as a working directory"}}"#,
            root2.display()
        );
        let mut content = std::fs::read_to_string(&transcript).expect("read");
        content.push('\n');
        content.push_str(&line2);
        std::fs::write(&transcript, content).expect("append");

        let result = router.dispatch(
            crate::hook::HookRequest::PreAgent {
                transcript_path: Some(transcript.to_string_lossy().to_string()),
                session_id: None,
            },
            0,
        );
        assert_eq!(result.add_roots, vec![root2.clone()]);

        // Stored set contains both.
        let stored = router.session.transcript_roots.lock().expect("lock");
        assert_eq!(stored.len(), 2);
        assert!(stored.contains(&root1));
        assert!(stored.contains(&root2));
    }
}
