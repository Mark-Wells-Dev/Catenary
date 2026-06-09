// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application dispatch for hook requests.
//!
//! `HookRouter` owns all hook method handlers and application logic
//! (editing state enforcement, diagnostics dispatch, turn tracking).
//! Protocol boundary delegates to router, router delegates to
//! [`super::session::Session`].

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
        "antigravity" => Some(crate::cli::HostFormat::Antigravity),
        _ => None,
    }
}

// ── Tool classification helpers ─────────────────────────────────────────

/// Returns `true` if the tool is an edit tool that requires `start_editing`.
///
/// Checks all known edit tool names across host CLIs (Claude Code, Gemini CLI,
/// and Antigravity CLI).
fn is_edit_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Edit"
            | "Write"
            | "NotebookEdit"
            | "write_file"
            | "replace"
            | "write_to_file"
            | "replace_file_content"
            | "multi_replace_file_content"
    )
}

/// Returns `true` if the tool is a read tool (always allowed during editing).
///
/// Checks all known read tool names across host CLIs.
fn is_read_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Read" | "NotebookRead" | "read_file")
}

/// Returns `true` if the tool is a shell tool (Bash, `run_shell_command`, or `run_command`).
fn is_bash_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Bash" | "run_shell_command" | "run_command")
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
/// `ToolSearch` must be allowed because Catenary tools are deferred in
/// Claude Code — blocking `ToolSearch` while editing prevents the agent
/// from loading tool schemas it may need.
///
/// Grep and glob are bypassed earlier in the hook chain (`run_pre_tool`)
/// before reaching editing enforcement, so they don't appear here.
fn is_allowed_during_editing(tool_name: &str) -> bool {
    tool_name == "ToolSearch"
}

/// Result of hook dispatch: the handler's result plus an optional
/// `systemMessage` from the notification queue drain.
pub struct DispatchResult {
    /// Handler result (`None` = allow / no actionable data).
    pub result: Option<HookResult>,
    /// Composed `systemMessage` content (direct + background drain).
    pub system_message: Option<String>,
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
            };
        };

        if !resolved.is_active() {
            return DispatchResult {
                result: None,
                system_message: None,
            };
        }

        let cwd_path = cwd.map(std::path::Path::new);
        let Some(denial) = crate::cli::command_filter::check_command(command, &resolved, cwd_path)
        else {
            return DispatchResult {
                result: None,
                system_message: None,
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
    pub(crate) fn dispatch(&self, request: HookRequest) -> DispatchResult {
        match request {
            HookRequest::PreAgent { session_id } => {
                self.store_client_session_id(session_id.as_deref());
                let turn = self.turn_counter.fetch_add(1, Ordering::AcqRel) + 1;
                debug!(
                    source = Source::HookDispatch.as_str(),
                    turn, "Hook: turn start"
                );

                DispatchResult {
                    result: None,
                    system_message: None,
                }
            }
            HookRequest::PreTool {
                tool_name,
                file_path,
                command,
                agent_id,
                session_id,
                ..
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
                DispatchResult {
                    result,
                    system_message: None,
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
                }
            }
            HookRequest::PreToolDoneEditingPrepare { session_id, .. } => {
                // Handled at the daemon level (router.rs), not here.
                // This arm exists for exhaustive matching in the
                // per-session HookServer path.
                self.store_client_session_id(session_id.as_deref());
                DispatchResult {
                    result: None,
                    system_message: None,
                }
            }
            HookRequest::DoneEditingRun => {
                // Handled at the daemon level (router.rs), not here.
                DispatchResult {
                    result: None,
                    system_message: None,
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
                }
            }
            HookRequest::SessionEnd { session_id } => {
                self.store_client_session_id(session_id.as_deref());
                // No-op at the router level — cleanup happens in the
                // daemon's handle_hook_dispatch (root tracker removal).
                DispatchResult {
                    result: None,
                    system_message: None,
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
    /// The first Edit/Write to a covered file *implicitly* enters editing
    /// mode and is allowed in the same invocation — there is no separate
    /// `editing start` step to race against parallel tool calls. Edits to
    /// files without known LSP coverage are always allowed and never enter
    /// editing mode (no diagnostics would be produced for them).
    ///
    /// The boundary block — denying non-edit commands until `catenary
    /// diagnostics` runs — gates on a **non-empty covered tracked set**, not
    /// the editing-mode bit (Decision 4). An empty set (doc-only / no-server
    /// edits, or an explicit `editing start` with no coverable edit yet) flows
    /// free: friction tracks value. While a covered set is pending, Read/Write,
    /// `ToolSearch`, filesystem-only Bash, and canonical Catenary commands
    /// (search/`sed`/lifecycle) stay allowed; everything else is blocked.
    fn handle_enforce_editing(
        &self,
        tool_name: &str,
        file_path: Option<&str>,
        command: Option<&str>,
        session_id: Option<&str>,
        agent_id: &str,
    ) -> Option<HookResult> {
        // Edit tools are handled identically whether or not editing mode is
        // already active. A covered edit implicitly enters editing mode and
        // acquires the per-root guardrail; an uncovered edit flows free without
        // entering editing mode. In-root files always have coverage; out-of-root
        // files have coverage only after a single-file server has successfully
        // initialized (positive cache). `start_editing` is idempotent, so a
        // covered edit arriving while already editing simply re-affirms the
        // mode and the root lock — parallel first-edits all succeed and none can
        // reject the others (race-free by construction).
        if is_edit_tool(tool_name) {
            if file_path.is_some_and(|p| !self.session.has_lsp_coverage(Path::new(p))) {
                return None;
            }
            // Cross-session guardrail before claiming the root: if another
            // session is editing it, deny without entering editing mode.
            if let Some(deny) = self.acquire_editing_guardrail(file_path) {
                return Some(deny);
            }
            let _ = self.session.editing.start_editing(session_id, agent_id);
            return None;
        }

        // Reads, `ToolSearch`, and filesystem-only Bash never produce code
        // diagnostics, so they are always allowed — independent of editing
        // state (`ToolSearch` must pass because Catenary tools are deferred in
        // Claude Code).
        if is_read_tool(tool_name)
            || is_allowed_during_editing(tool_name)
            || (is_bash_tool(tool_name) && command.is_some_and(is_filesystem_only_bash))
        {
            return None;
        }

        // Boundary block gates on a non-empty *covered* tracked set, not the
        // mode bit. Empty set ⇒ nothing to diagnose ⇒ flow free.
        if !self.session.editing.has_files(session_id, agent_id) {
            return None;
        }

        // A Catenary command reaching the boundary (the client-side
        // canonical-form matcher normally intercepts these) is classified by
        // the matcher rather than the generic boundary block, which would echo
        // the command the agent just ran (bugs/16). Canonical search/`sed`/
        // lifecycle commands are allowed during editing; a non-canonical form
        // gets the matcher's clear message.
        if is_bash_tool(tool_name) {
            use crate::cli::command_filter::CatenaryAction;
            match command.map(crate::cli::command_filter::analyze_catenary_command) {
                Some(CatenaryAction::Deny(msg)) => return Some(HookResult::Deny(msg)),
                Some(
                    CatenaryAction::EditingStart
                    | CatenaryAction::Diagnostics
                    | CatenaryAction::Sed { .. }
                    | CatenaryAction::Allow { .. },
                ) => return None,
                Some(CatenaryAction::NotCatenary) | None => {}
            }
        }

        Some(HookResult::Deny(self.boundary_block_message(
            session_id, agent_id, command, tool_name,
        )))
    }

    /// Build the intent-neutral boundary-block deny message.
    ///
    /// Names the blocked command (the shell command when present, else the tool
    /// name), lists the covered files currently tracked for diagnostics, and
    /// points at `catenary diagnostics`. It carries no inferred intent
    /// ("before testing"/"before building" are guesses and often wrong) — it
    /// anchors only on what Catenary knows: coverable edits exist and have not
    /// been diagnosed.
    fn boundary_block_message(
        &self,
        session_id: Option<&str>,
        agent_id: &str,
        command: Option<&str>,
        tool_name: &str,
    ) -> String {
        use std::fmt::Write as _;
        let what = command.unwrap_or(tool_name);
        let mut msg = format!(
            "command `{what}` is blocked\n\
             The following files are tracked via a language server:\n"
        );
        for file in self.session.editing.files(session_id, agent_id) {
            let _ = writeln!(msg, "  {}", file.display());
        }
        msg.push_str("Call `catenary diagnostics` to proceed.");
        msg
    }

    /// Acquires the cross-session editing guardrail for `file_path`'s root.
    ///
    /// Returns `Some(Deny)` with guidance when another session holds the
    /// lock on that root; otherwise `None` (lock acquired or re-affirmed
    /// for this session, or there is no guardrail / no resolvable root).
    /// Locks are acquired lazily per-root, so only roots with actual edits
    /// are locked.
    fn acquire_editing_guardrail(&self, file_path: Option<&str>) -> Option<HookResult> {
        if let Some(guardrail) = &self.session.editing_guardrail
            && let Some(root) = file_path
                .map(Path::new)
                .and_then(|p| self.session.resolve_root(p))
            && let Err(msg) = guardrail.try_acquire(&root, &self.session.instance_id)
        {
            return Some(HookResult::Deny(msg));
        }
        None
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
        if !self.session.editing.is_editing(session_id, agent_id)
            || !tool_name.is_some_and(is_edit_tool)
        {
            return None;
        }

        // Only accumulate files with known LSP coverage — files
        // without coverage have no server to produce diagnostics,
        // so processing them in done_editing is wasted work.
        let path = Path::new(file_path);
        if self.session.has_lsp_coverage(path) {
            self.session
                .editing
                .add_file(session_id, agent_id, PathBuf::from(file_path));
            debug!(
                source = Source::HookDispatch.as_str(),
                file = file_path,
                "file accumulated for diagnostics",
            );
        } else {
            self.session.editing.increment_filtered();
            debug!(
                source = Source::HookDispatch.as_str(),
                file = file_path,
                "file filtered (no LSP coverage)",
            );
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
            if self.session.editing.has_files(session_id, agent_id) {
                Some(HookResult::Block(
                    "run `catenary diagnostics` before finishing".into(),
                ))
            } else {
                // No files modified — silently clear editing state.
                self.session.editing.done_editing(session_id, agent_id);
                if let Some(guardrail) = &self.session.editing_guardrail {
                    guardrail.release_all(&self.session.instance_id);
                }
                None
            }
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
        // Antigravity CLI edit tools
        assert!(is_edit_tool("write_to_file"));
        assert!(is_edit_tool("replace_file_content"));
        assert!(is_edit_tool("multi_replace_file_content"));
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
        assert!(!is_read_tool("run_command"));
    }

    #[test]
    fn test_is_allowed_during_editing() {
        // ToolSearch (Claude Code deferred tool loader)
        assert!(is_allowed_during_editing("ToolSearch"));
        // Grep/glob are now CLI commands — bypassed in the hook
        // chain before reaching editing enforcement.
        assert!(!is_allowed_during_editing("grep"));
        assert!(!is_allowed_during_editing("glob"));
        assert!(!is_allowed_during_editing("Bash"));
        // Unrelated tools
        assert!(!is_allowed_during_editing("Edit"));
        assert!(!is_allowed_during_editing("Write"));
    }

    // ── Handler tests ───────────────────────────────────────────────────

    #[test]
    fn no_explicit_start_required() {
        let (router, root) = test_router_with_root();
        // No prior `editing start`. The old behavior denied an in-root
        // edit with "run `catenary editing start`"; implicit start now
        // allows it.
        let in_root = format!("{}/src/main.rs", root.display());
        let result = router.handle_enforce_editing("Edit", Some(&in_root), None, None, "");
        assert!(
            result.is_none(),
            "in-root edit should be allowed without explicit start, got {result:?}"
        );
    }

    #[test]
    fn test_hook_enforce_editing_allow() {
        let router = test_router();
        // Enter editing mode with a covered file pending — the boundary block
        // gates on a non-empty tracked set, so a file must be accumulated for a
        // non-edit command to be denied.
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .add_file(None, "", PathBuf::from("/src/main.rs"));

        // Edit tool — should allow during editing mode
        let result = router.handle_enforce_editing("Edit", None, None, None, "");
        assert!(result.is_none(), "expected allow, got {result:?}");

        // Read tool — always allowed during editing
        let result = router.handle_enforce_editing("Read", None, None, None, "");
        assert!(result.is_none(), "expected allow for Read, got {result:?}");

        // Non-edit, non-read tool with a covered set pending — should deny
        let result = router.handle_enforce_editing("Bash", None, None, None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny for Bash, got {result:?}");
        };
        assert!(reason.contains("diagnostics"));
    }

    #[test]
    fn test_hook_enforce_editing_piped_catenary_bugs16() {
        let router = test_router();
        // A covered file must be pending for the boundary block to fire.
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .add_file(None, "", PathBuf::from("/src/main.rs"));

        // bugs/16: a piped lifecycle command during editing must get a clear
        // pipe-deny from the canonical-form matcher, not the boundary block
        // that echoes the command the agent just ran.
        let result = router.handle_enforce_editing(
            "Bash",
            None,
            Some("catenary editing start | head"),
            None,
            "",
        );
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny, got {result:?}");
        };
        assert!(
            !reason.contains("is blocked"),
            "should not be the boundary block, got: {reason}"
        );
        assert!(
            reason.contains("bare") || reason.contains("owns its output"),
            "should be the matcher pipe-deny, got: {reason}"
        );

        // A bare canonical catenary command is allowed during editing.
        let result =
            router.handle_enforce_editing("Bash", None, Some("catenary editing start"), None, "");
        assert!(
            result.is_none(),
            "bare editing start allowed during editing, got {result:?}"
        );

        // `catenary diagnostics` (the renamed boundary command) is likewise
        // allowed during editing — it ends the session and prints diagnostics.
        let result =
            router.handle_enforce_editing("Bash", None, Some("catenary diagnostics"), None, "");
        assert!(
            result.is_none(),
            "bare diagnostics allowed during editing, got {result:?}"
        );

        // A foreign non-edit command still hits the boundary block, which now
        // names the command and lists the tracked file.
        let result = router.handle_enforce_editing("Bash", None, Some("make test"), None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny for make test, got {result:?}");
        };
        assert!(
            reason.contains("`make test` is blocked") && reason.contains("catenary diagnostics"),
            "foreign cmd → boundary block, got: {reason}"
        );
    }

    #[test]
    fn test_hook_file_accumulation() {
        let (router, root) = test_router_with_root();
        let _ = router.session.editing.start_editing(None, "");

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
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .add_file(None, "", PathBuf::from("/src/main.rs"));

        let result = router.handle_require_release(None, "", false);
        let Some(HookResult::Block(reason)) = result else {
            unreachable!("expected Block, got {result:?}");
        };
        assert!(reason.contains("diagnostics"));
    }

    #[test]
    fn test_hook_require_release_no_files_allows() {
        let router = test_router();
        let _ = router.session.editing.start_editing(None, "");

        // Editing mode active but no files modified — should allow
        let result = router.handle_require_release(None, "", false);
        assert!(result.is_none(), "expected allow, got {result:?}");

        // Editing state should be cleared
        assert!(
            !router.session.editing.is_editing(None, ""),
            "editing state should be cleared when no files pending"
        );
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
        let _ = router.session.editing.start_editing(None, "");

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
        let _ = router.session.editing.start_editing(None, "");
        let _ = router.session.editing.start_editing(None, "agent-b");

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

        let result = router.dispatch(crate::hook::HookRequest::PreToolStartEditing {
            agent_id: String::new(),
            session_id: None,
        });
        assert!(result.result.is_none(), "start_editing should allow");
        assert!(
            router.session.editing.is_editing(None, ""),
            "should be in editing mode after dispatch"
        );
    }

    #[test]
    fn dispatch_start_editing_cli_with_agent_id() {
        let router = test_router();
        let result = router.dispatch(crate::hook::HookRequest::PreToolStartEditing {
            agent_id: "sub-agent".to_string(),
            session_id: None,
        });
        assert!(result.result.is_none());
        assert!(router.session.editing.is_editing(None, "sub-agent"));
        assert!(!router.session.editing.is_editing(None, ""));
    }

    #[test]
    fn dispatch_start_editing_cli_then_edit_allowed() {
        let (router, root) = test_router_with_root();
        // Enter editing via the CLI path.
        router.dispatch(crate::hook::HookRequest::PreToolStartEditing {
            agent_id: String::new(),
            session_id: None,
        });

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
    fn first_edit_enters_editing_mode() {
        let (router, root) = test_router_with_root();
        let in_root = format!("{}/src/main.rs", root.display());
        assert!(
            !router.session.editing.is_editing(None, ""),
            "not editing before first edit"
        );

        // A single Edit with no prior start, dispatched end-to-end:
        // enforce enters editing mode, then accumulation tracks the file.
        let res = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(in_root.clone()),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });
        assert!(
            res.result.is_none(),
            "first edit allowed, got {:?}",
            res.result
        );
        assert!(
            router.session.editing.is_editing(None, ""),
            "editing mode entered by the first edit"
        );

        let files = router.session.editing.drain_files(None, "");
        assert_eq!(
            files,
            vec![PathBuf::from(&in_root)],
            "covered file accumulated"
        );
    }

    #[test]
    fn parallel_edits_both_succeed() {
        let (router, root) = test_router_with_root();
        let f1 = format!("{}/src/a.rs", root.display());
        let f2 = format!("{}/src/b.rs", root.display());

        // Two first-edits for the same session, no prior `editing start`.
        // Dispatched back-to-back; both must be allowed and editing mode
        // must be entered exactly once (the idempotent set never rejects).
        let r1 = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(f1.clone()),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });
        let r2 = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(f2.clone()),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });

        assert!(
            r1.result.is_none(),
            "first edit allowed, got {:?}",
            r1.result
        );
        assert!(
            r2.result.is_none(),
            "second edit allowed, got {:?}",
            r2.result
        );
        assert!(router.session.editing.is_editing(None, ""));

        // Both files land under a single (session, agent) entry.
        let files = router.session.editing.drain_files(None, "");
        assert_eq!(files, vec![PathBuf::from(&f1), PathBuf::from(&f2)]);
        assert_eq!(
            router.session.editing.clear_all(),
            1,
            "mode entered exactly once"
        );
    }

    #[test]
    fn uncovered_file_no_mode_no_gate() {
        // No roots → no LSP coverage for the edited file.
        let router = test_router();
        let res = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some("/outside/some/file.rs".to_string()),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });
        assert!(
            res.result.is_none(),
            "uncovered edit allowed, got {:?}",
            res.result
        );
        assert!(
            !router.session.editing.is_editing(None, ""),
            "uncovered edit must not enter editing mode"
        );
        let (files, filtered) = router.session.editing.drain_all_and_clear();
        assert!(files.is_empty(), "uncovered file not accumulated");
        // Not in editing mode → accumulation returns early, so the file is
        // not even counted as filtered.
        assert_eq!(filtered, 0);
    }

    #[test]
    fn no_file_path_edit_enters_editing() {
        let router = test_router();
        // Edit with no file path (e.g., host didn't supply it): there is
        // no path to test for coverage, so the edit is allowed and enters
        // editing mode like any other first edit.
        let result = router.handle_enforce_editing("Edit", None, None, None, "");
        assert!(
            result.is_none(),
            "edit with no file path should be allowed, got {result:?}"
        );
        assert!(router.session.editing.is_editing(None, ""));
    }

    #[test]
    fn test_file_accumulation_skips_out_of_root() {
        let router = test_router();
        let _ = router.session.editing.start_editing(None, "");

        // File outside workspace roots — should not be accumulated
        // but should increment filtered counter.
        router.handle_file_accumulation("/outside/some/file.rs", None, "", Some("Edit"));
        let files = router.session.editing.drain_files(None, "");
        assert!(
            files.is_empty(),
            "out-of-root file should not be accumulated"
        );
        let (_, filtered) = router.session.editing.drain_all_and_clear();
        assert_eq!(filtered, 1, "out-of-root edit should increment filtered");
    }

    #[test]
    fn test_file_accumulation_keeps_in_root() {
        let (router, root) = test_router_with_root();
        let _ = router.session.editing.start_editing(None, "");

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
            None,
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
    fn single_file_covered_edit_enters_editing() {
        // single_file = true, no failure → server expected to cover it →
        // implicit start (previously gated with "catenary editing start").
        let router = test_router_with_sf_config(false);
        let path = format!("/outside/file.{SF_LANG}");
        let result = router.handle_enforce_editing("Edit", Some(&path), None, None, "");
        assert!(
            result.is_none(),
            "single_file-covered edit should enter editing, got {result:?}"
        );
        assert!(router.session.editing.is_editing(None, ""));
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
        let _ = router.session.editing.start_editing(None, "");

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
        let _ = router.session.editing.start_editing(None, "");

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
        assert!(is_bash_tool("run_command")); // Antigravity CLI
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
        // A covered file is pending — filesystem-only Bash is still allowed.
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .add_file(None, "", PathBuf::from("/src/main.rs"));

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
        // Covered set pending → non-filesystem Bash is gated.
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .add_file(None, "", PathBuf::from("/src/main.rs"));

        // Non-filesystem Bash — should deny during editing
        let result = router.handle_enforce_editing("Bash", None, Some("cargo build"), None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected Deny for non-filesystem Bash, got {result:?}");
        };
        assert!(reason.contains("diagnostics"));
    }

    #[test]
    fn test_enforce_editing_denies_bash_without_command() {
        let router = test_router();
        // Covered set pending → a Bash call we cannot inspect must deny.
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .add_file(None, "", PathBuf::from("/src/main.rs"));

        // Bash without command string — cannot verify, must deny
        let result = router.handle_enforce_editing("Bash", None, None, None, "");
        let Some(HookResult::Deny(_)) = result else {
            unreachable!("expected Deny for Bash without command, got {result:?}");
        };
    }

    // ── Boundary block (covered-set gate) tests ─────────────────────────

    #[test]
    fn boundary_blocks_on_covered_set() {
        let (router, root) = test_router_with_root();
        let in_root = format!("{}/src/main.rs", root.display());

        // A covered edit (dispatched end-to-end) enters editing mode and
        // accumulates the file.
        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(in_root.clone()),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });
        assert!(router.session.editing.has_files(None, ""));

        // A non-edit command is now blocked; the message names the command,
        // lists the tracked file, and points at `catenary diagnostics`.
        let result = router.handle_enforce_editing("Bash", None, Some("make test"), None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected boundary block, got {result:?}");
        };
        assert!(
            reason.contains("`make test` is blocked"),
            "message should name the command, got: {reason}"
        );
        assert!(
            reason.contains(&in_root),
            "message should list the tracked file, got: {reason}"
        );
        assert!(
            reason.contains("catenary diagnostics"),
            "message should name `catenary diagnostics`, got: {reason}"
        );
    }

    #[test]
    fn empty_set_not_blocked() {
        // Editing mode active (e.g. an explicit `editing start`) but no covered
        // edit yet → the boundary block must not fire.
        let router = test_router();
        let _ = router.session.editing.start_editing(None, "");
        assert!(!router.session.editing.has_files(None, ""));

        let result = router.handle_enforce_editing("Bash", None, Some("make test"), None, "");
        assert!(
            result.is_none(),
            "empty covered set should flow free, got {result:?}"
        );
    }

    #[test]
    fn doc_only_edit_flows_free() {
        // A no-server file outside every root is uncovered: the edit never
        // enters editing mode, so a following non-edit command flows free.
        let router = test_router();
        let res = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some("/outside/notes.md".to_string()),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });
        assert!(res.result.is_none(), "uncovered edit allowed");
        assert!(
            !router.session.editing.has_files(None, ""),
            "uncovered edit must not accumulate a covered set"
        );

        let result = router.handle_enforce_editing("Bash", None, Some("make test"), None, "");
        assert!(
            result.is_none(),
            "doc-only edit should leave the boundary unblocked, got {result:?}"
        );
    }

    #[test]
    fn catenary_subcommands_not_blocked() {
        let (router, root) = test_router_with_root();
        let in_root = format!("{}/src/main.rs", root.display());
        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(in_root),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });
        assert!(router.session.editing.has_files(None, ""));

        // Canonical Catenary commands stay allowed mid-editing even with a
        // covered set pending: search, the renamed boundary command, and the
        // `sed --in-place` edit op (ticket 08).
        for cmd in [
            "catenary grep needle",
            "catenary glob foo.rs",
            "catenary diagnostics",
            "catenary sed --in-place foo bar src/main.rs",
        ] {
            let result = router.handle_enforce_editing("Bash", None, Some(cmd), None, "");
            assert!(
                result.is_none(),
                "`{cmd}` should not be blocked mid-editing, got {result:?}"
            );
        }
    }

    #[test]
    fn coverage_resolves_to_root_instance() {
        // The test harness spawns no LSP server instances, so coverage must
        // resolve from root membership alone — a warm language's cold per-root
        // instance must not silently drop the file (Decision 3 granularity).
        let (router, root) = test_router_with_root();
        let in_root = format!("{}/src/main.rs", root.display());
        assert!(
            router.session.has_lsp_coverage(Path::new(&in_root)),
            "in-root file must be covered with no running instance"
        );

        // The edit is therefore tracked, arming the boundary block.
        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(in_root),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });
        assert!(
            router.session.editing.has_files(None, ""),
            "covered in-root edit accumulated despite cold per-root instance"
        );
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
            None,
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
            None,
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

    /// Create a daemon-mode `HookRouter` with a cross-session editing
    /// guardrail and one workspace root.
    ///
    /// Returns the router, the root, and the shared guardrail so a test can
    /// pre-acquire a foreign lock to exercise the cross-session deny path.
    fn test_router_with_guardrail() -> (
        TestHookRouter,
        PathBuf,
        Arc<crate::bridge::editing_guardrail::EditingGuardrail>,
    ) {
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
        // The primary session owns the shared resources (fs_manager carries
        // the workspace root); the per-session daemon session carries the
        // guardrail under test.
        let primary = Session::new(
            config,
            vec![root.clone()],
            logging,
            conn.clone(),
            instance_id.clone(),
            handle,
            notification_router,
            None,
        );
        let guardrail = Arc::new(crate::bridge::editing_guardrail::EditingGuardrail::new());
        let session = Arc::new(Session::new_for_daemon(
            &primary,
            instance_id.clone(),
            Some(guardrail.clone()),
        ));

        let router = HookRouter::new(session, conn, instance_id, "test".to_string());

        (
            TestHookRouter {
                _dir: dir,
                _runtime: runtime,
                router,
            },
            root,
            guardrail,
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

        let result = router.dispatch(crate::hook::HookRequest::SessionStart { session_id: None });
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
        let result = router.dispatch(crate::hook::HookRequest::PostAgent {
            agent_id: String::new(),
            session_id: None,
            stop_hook_active: false,
        });
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
        let _ = router.session.editing.start_editing(None, "");

        // Add a file so editing mode has pending work.
        router
            .session
            .editing
            .add_file(None, "", PathBuf::from("/src/main.rs"));

        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );

        let result = router.dispatch(crate::hook::HookRequest::PostAgent {
            agent_id: String::new(),
            session_id: None,
            stop_hook_active: false,
        });
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

        let result = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Read".to_string(),
            file_path: None,
            command: None,
            agent_id: String::new(),
            session_id: None,
        });
        assert!(result.system_message.is_none(), "pre-tool should not drain");
        assert_eq!(
            router.session.notification_router.queue_len("test-session"),
            1
        );
    }

    #[test]
    fn dispatch_stop_block_then_allow_drains_accumulated() {
        let router = test_router();
        // Enter editing mode with a file so stop blocks.
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .add_file(None, "", PathBuf::from("/src/main.rs"));

        // Enqueue a notification before the first stop.
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );

        // First stop: block (editing active) — queue preserved.
        let result = router.dispatch(crate::hook::HookRequest::PostAgent {
            agent_id: String::new(),
            session_id: None,
            stop_hook_active: false,
        });
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
        let result = router.dispatch(crate::hook::HookRequest::PostAgent {
            agent_id: String::new(),
            session_id: None,
            stop_hook_active: true,
        });
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
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .add_file(None, "", PathBuf::from("/src/main.rs"));

        // Enqueue a notification.
        crate::logging::Sink::handle(
            router.session.notification_router.as_ref(),
            &make_notify_event("server offline", "ra"),
        );

        // Block — queue preserved.
        let result = router.dispatch(crate::hook::HookRequest::PostAgent {
            agent_id: String::new(),
            session_id: None,
            stop_hook_active: false,
        });
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
        let result = router.dispatch(crate::hook::HookRequest::PostAgent {
            agent_id: String::new(),
            session_id: None,
            stop_hook_active: true,
        });
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
            parent_id: None,
            source: None,
            language: None,
            payload: None,
            scope_root: None,
            session_id: Some("test-session".to_string()),
            fields: serde_json::Map::new(),
        }
    }

    // ── Turn counter tests ────────────────────────────────────────────

    #[test]
    fn turn_counter_increments_on_dispatch() {
        let router = test_router();
        assert_eq!(router.turn(), 0);

        router.dispatch(crate::hook::HookRequest::PreAgent { session_id: None });
        assert_eq!(router.turn(), 1);

        router.dispatch(crate::hook::HookRequest::PreAgent { session_id: None });
        assert_eq!(router.turn(), 2);
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
            None,
        ));
        let router = HookRouter::new(session, conn, instance_id, "test".to_string());
        TestHookRouter {
            _dir: dir,
            _runtime: runtime,
            router,
        }
    }

    fn dispatch_check_denied(router: &HookRouter) -> DispatchResult {
        router.dispatch(crate::hook::HookRequest::CheckCommand {
            command: "cargo test".to_string(),
            cwd: None,
            session_id: None,
            format: None,
        })
    }

    fn dispatch_check_allowed(router: &HookRouter) -> DispatchResult {
        router.dispatch(crate::hook::HookRequest::CheckCommand {
            command: "git status".to_string(),
            cwd: None,
            session_id: None,
            format: None,
        })
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
        router.dispatch(crate::hook::HookRequest::PreAgent { session_id: None });
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
        let _ = router.session.editing.start_editing(None, "");

        let file = format!("{}/src/main.rs", root.display());
        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(file.clone()),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });

        let files = router.session.editing.drain_files(None, "");
        assert_eq!(
            files,
            vec![PathBuf::from(&file)],
            "PreTool for edit tool should accumulate file"
        );
    }

    #[test]
    fn dispatch_pre_tool_denied_does_not_accumulate() {
        // A guardrail-denied edit (another session owns the root) must be
        // denied without entering editing mode or accumulating the file.
        let (router, root, guardrail) = test_router_with_guardrail();
        guardrail
            .try_acquire(&root, "other-session")
            .expect("foreign lock acquired");

        let file = format!("{}/src/main.rs", root.display());
        let result = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(file),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });

        assert!(
            matches!(result.result, Some(HookResult::Deny(_))),
            "edit on a root owned by another session should be denied"
        );
        assert!(
            !router.session.editing.is_editing(None, ""),
            "denied first edit must not enter editing mode"
        );
        let (files, _) = router.session.editing.drain_all_and_clear();
        assert!(files.is_empty(), "denied edit should not accumulate file");
    }

    #[test]
    fn dispatch_pre_tool_non_edit_does_not_accumulate() {
        let (router, root) = test_router_with_root();
        let _ = router.session.editing.start_editing(None, "");

        let file = format!("{}/src/main.rs", root.display());
        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "mcp_catenary_grep".to_string(),
            file_path: Some(file),
            command: None,
            agent_id: String::new(),
            session_id: None,
        });

        let files = router.session.editing.drain_files(None, "");
        assert!(files.is_empty(), "non-edit tool should not accumulate file");
    }
}
