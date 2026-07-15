// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Application dispatch for hook requests.
//!
//! `HookRouter` owns all hook method handlers and application logic
//! (editing state enforcement, diagnostics dispatch). Protocol boundary
//! delegates to router, router delegates to [`super::session::Session`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

use crate::source::Source;

use super::session::Session;
use crate::hook::{HookRequest, HookResult};

// ── Tool classification helpers ─────────────────────────────────────────

/// Returns `true` if the tool is an edit tool that requires `start_editing`.
///
/// Checks all known edit tool names across host CLIs (Claude Code, Antigravity
/// CLI, and OpenCode — whose built-ins are lowercase: `edit`/`write`/`patch`).
#[must_use]
pub fn is_edit_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "Edit"
            | "Write"
            | "NotebookEdit"
            | "write_to_file"
            | "replace_file_content"
            | "multi_replace_file_content"
            | "edit"
            | "write"
            | "patch"
    )
}

/// Returns `true` if the tool is a read tool (always allowed during editing).
///
/// Checks all known read tool names across host CLIs (OpenCode's built-in is
/// lowercase `read`).
fn is_read_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Read" | "NotebookRead" | "read_file" | "read")
}

/// Returns `true` if the tool is a shell tool (`Bash`, `run_command`, or
/// OpenCode's lowercase `bash`).
fn is_bash_tool(tool_name: &str) -> bool {
    matches!(tool_name, "Bash" | "run_command" | "bash")
}

/// The display name the skipped-edits note uses for an in-root file no feeder
/// covers (misc 173): its final path component, falling back to the full path
/// when there is none.
fn skipped_display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
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
/// `additionalContext` payload for the parent agent (misc 151).
///
/// The user-facing `systemMessage` notification queue retired (tui-rework 04);
/// what rides the hook response now is the parent-agent leg — the dirty-worktree
/// notice drained from the [`ParentContextQueue`](super::parent_context::ParentContextQueue)
/// on the parent's next eligible response.
pub struct DispatchResult {
    /// Handler result (`None` = allow / no actionable data).
    pub result: Option<HookResult>,
    /// Drained `additionalContext` for the parent agent, delivered when the
    /// response allows. `None` = no `additionalContext` field in host output.
    pub additional_context: Option<String>,
}

// ── HookRouter ──────────────────────────────────────────────────────────

/// Application dispatch for hook requests.
///
/// Routes parsed [`HookRequest`] values to the appropriate handler and
/// returns an optional [`HookResult`]. Holds all shared application state
/// needed by hook handlers: editing state (via [`super::editing_manager::EditingManager`]
/// on [`Session`]).
pub struct HookRouter {
    pub(crate) session: Arc<Session>,
    #[allow(
        dead_code,
        reason = "per-session router identity; the client-session DB write that read it was removed (observability ticket 07)"
    )]
    instance_id: Arc<str>,
    /// Host CLI client name (e.g., `"host"`, `"claude-code"`).
    pub(crate) client_name: String,
}

impl HookRouter {
    /// Creates a new `HookRouter`.
    #[must_use]
    pub const fn new(session: Arc<Session>, instance_id: Arc<str>, client_name: String) -> Self {
        Self {
            session,
            instance_id,
            client_name,
        }
    }

    /// Dispatches a parsed hook request to the appropriate handler.
    ///
    /// Returns a [`DispatchResult`] with the handler's result and, when the
    /// response allows, any `additionalContext` drained from the parent-agent
    /// [`ParentContextQueue`](super::parent_context::ParentContextQueue). The
    /// queue is drained only for the *parent* (empty `agent_id`) on its eligible
    /// hook exchanges — `PreToolUse` and `Stop`/`AfterAgent` — so a subagent's
    /// own hook traffic never absorbs a notice meant for its spawner (misc 151).
    #[allow(clippy::too_many_lines, reason = "match arms are sequential and flat")]
    pub(crate) fn dispatch(&self, request: HookRequest) -> DispatchResult {
        match request {
            HookRequest::PreTool {
                tool_name,
                file_path,
                command,
                agent_id,
                session_id,
                writes,
            } => {
                let result = self.handle_enforce_editing(
                    &tool_name,
                    file_path.as_deref(),
                    command.as_deref(),
                    session_id.as_deref(),
                    &agent_id,
                );
                // Accumulate only when the tool call is allowed (`result` is
                // `None`) — a denied call must accumulate nothing (mirrors the
                // Edit/Write guardrail-deny invariant, extended to opaque-write
                // denials, which the client rejects before this dispatch).
                if result.is_none() {
                    // File tracking for Edit/Write tools.
                    if let Some(ref path) = file_path {
                        // Any touched file (read or edit) makes its language
                        // activity-live for the health dashboard — the
                        // filetype-open gate (tui-rework 09, item 5), independent
                        // of diagnostics coverage below.
                        self.session.record_activity_touch(Path::new(path));
                        self.handle_file_accumulation(
                            path,
                            session_id.as_deref(),
                            &agent_id,
                            Some(&tool_name),
                        );
                    }
                    // Resolved shell writes attribute exactly like edits
                    // (ws38 ticket 02): covered targets enter the caller's
                    // modified-set, the first one entering editing mode.
                    //
                    // `catenary worktree land` is the one exception (misc 189):
                    // its resolved write-set is the whole landed diff, but the
                    // ruling says debt *transfers* from the owner, never
                    // duplicates. So a land arms only the subset of the landed
                    // files whose owner left them UNPAID — read from the owner's
                    // ledger, not the git content. A worktree whose worker paid
                    // (or whose batch died with a bounced daemon, bug 79) arms
                    // nothing.
                    if !writes.is_empty() {
                        for write in &writes {
                            self.session.record_activity_touch(write);
                        }
                        if let Some(land_path) = command
                            .as_deref()
                            .and_then(crate::cli::command_filter::worktree_land_path)
                        {
                            self.handle_worktree_land_debt_transfer(
                                &land_path,
                                &writes,
                                session_id.as_deref(),
                                &agent_id,
                            );
                        } else {
                            self.handle_shell_write_accumulation(
                                &writes,
                                session_id.as_deref(),
                                &agent_id,
                            );
                        }
                    }
                }
                // Deliver any pending parent-agent context on an allowed
                // PreToolUse — the parent's most frequent eligible response
                // (misc 151). A denied call carries nothing.
                let additional_context = if result.is_none() {
                    self.drain_parent_context(&agent_id)
                } else {
                    None
                };
                DispatchResult {
                    result,
                    additional_context,
                }
            }
            HookRequest::PreToolStartEditing {
                agent_id,
                session_id,
            } => {
                let _ = self
                    .session
                    .editing
                    .start_editing(session_id.as_deref(), &agent_id);
                // Status may flip to `editing` — refresh the board (ticket 05).
                self.session.touch_snapshot();
                DispatchResult {
                    result: None,
                    additional_context: None,
                }
            }
            HookRequest::PreToolDoneEditingPrepare { .. } => {
                // Handled at the daemon level (router.rs), not here.
                // This arm exists for exhaustive matching in the
                // per-session HookServer path.
                DispatchResult {
                    result: None,
                    additional_context: None,
                }
            }
            HookRequest::DoneEditingRun => {
                // Handled at the daemon level (router.rs), not here.
                DispatchResult {
                    result: None,
                    additional_context: None,
                }
            }
            HookRequest::PostAgent {
                agent_id,
                session_id,
                stop_hook_active,
            } => {
                let result =
                    self.handle_require_release(session_id.as_deref(), &agent_id, stop_hook_active);
                // Editing state may have cleared (status → idle) — refresh the
                // board (ticket 05).
                self.session.touch_snapshot();
                // Deliver any pending parent-agent context on an allowed Stop —
                // the second eligible delivery point (misc 151). A blocked stop
                // (editing gate / lingering-worktree nag) carries nothing.
                let additional_context = if matches!(result, Some(HookResult::Block(_))) {
                    None
                } else {
                    self.drain_parent_context(&agent_id)
                };
                DispatchResult {
                    result,
                    additional_context,
                }
            }
            HookRequest::SessionStart { session_id: _ } => {
                let result = self.handle_clear_editing();
                // Stale editing state may have cleared (status → idle) —
                // refresh the board (ticket 05).
                self.session.touch_snapshot();
                DispatchResult {
                    result,
                    additional_context: None,
                }
            }
            HookRequest::SessionEnd { session_id: _ } => {
                // No-op at the router level — cleanup happens in the
                // daemon's handle_hook_dispatch (root tracker removal).
                DispatchResult {
                    result: None,
                    additional_context: None,
                }
            }
        }
    }

    /// Drain the parent-agent [`ParentContextQueue`](super::parent_context::ParentContextQueue)
    /// into an `additionalContext` string (misc 151, D-1).
    ///
    /// Only the **parent** (empty `agent_id` — the top-level agent) drains: a
    /// sibling subagent's hook traffic shares the `session_id` but must not
    /// absorb a notice meant for the spawner. Multiple queued lines join with
    /// newlines.
    /// Returns `None` when the queue is empty (the common case) or when a
    /// subagent is calling.
    fn drain_parent_context(&self, agent_id: &str) -> Option<String> {
        if !agent_id.is_empty() {
            return None;
        }
        let lines = self.session.parent_context.drain(&self.session.instance_id);
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
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
    /// diagnostics` runs — gates on **any undelivered covered file in the batch**,
    /// not the editing-mode bit (Decision 4). A batch with nothing undelivered (no
    /// coverable edit yet, or one already fully diagnosed) flows free: friction
    /// tracks value. While undelivered debt is pending, Read/Write, `ToolSearch`,
    /// filesystem-only Bash, and canonical Catenary commands (search/lifecycle)
    /// stay allowed; everything else is blocked.
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
            // Canonicalize the edit path (when it exists) BEFORE the coverage check
            // and the guardrail: roots are canonical, so a symlinked-prefix path
            // (macOS `$TMPDIR` → `/private/var/…`) would otherwise fail both the
            // roots prefix test — an in-root edit mis-classed as uncovered, so the
            // gate lets it flow free without entering editing mode — and the
            // guardrail's `resolve_root`, silently skipping the cross-session lock.
            // Matches the accumulation path (`handle_file_accumulation`). A
            // not-yet-existing edit target keeps its spelling.
            let canonical = file_path.map(|p| {
                Path::new(p)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(p))
            });
            if canonical
                .as_deref()
                .is_some_and(|p| !self.session.covered_for_diagnostics(p))
            {
                return None;
            }
            // Cross-session guardrail before claiming the root: if another
            // session is editing it, deny without entering editing mode.
            if let Some(deny) = self.acquire_editing_guardrail(canonical.as_deref()) {
                return Some(deny);
            }
            // The first covered edit implicitly enters editing mode. `Ok` means
            // this call created the editing entry (not a re-affirm by a parallel
            // edit), so promote exactly one `editing_start` milestone per editing
            // batch (ticket 08).
            if self
                .session
                .editing
                .start_editing(session_id, agent_id)
                .is_ok()
            {
                self.session.record_milestone(
                    crate::state_snapshot::MilestoneKind::EditingStart,
                    "editing started",
                    session_id.map(str::to_string),
                );
            }
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

        // Reconcile the batch against disk before consulting the gate (bug 76):
        // a write set is resolved and recorded at `PreToolUse`, before the
        // command runs, so a command that then fails wholesale leaves phantom
        // targets that never came to exist. Dropping them here means a failed
        // write cannot gate future work on files nothing ever touched, while a
        // real edit that was written and later deleted survives (it was observed
        // on disk once) to gate and be reported honestly.
        self.session
            .editing
            .reconcile(session_id, agent_id, Path::exists);

        // Boundary block gates on undelivered covered debt, not the mode bit.
        // Nothing undelivered ⇒ nothing to diagnose ⇒ flow free (misc 141).
        if !self.session.editing.has_undelivered(session_id, agent_id) {
            return None;
        }

        // A Catenary command reaching the boundary (the client-side
        // canonical-form matcher normally intercepts these) is classified by
        // the matcher rather than the generic boundary block, which would echo
        // the command the agent just ran (bugs/16). Canonical search/lifecycle
        // commands are allowed during editing; a non-canonical form gets the
        // matcher's clear message.
        if is_bash_tool(tool_name) {
            use crate::cli::command_filter::CatenaryAction;
            // No declared client here: the daemon-side boundary classifier has
            // no hook-definition identity, and the client-side matcher already
            // carries the client-keyed denials (misc 177) before any IPC — so
            // `None` classifies neutrally and never over-denies for hosts
            // whose hook set lacks WorktreeCreate.
            match command.map(|c| crate::cli::command_filter::analyze_catenary_command(c, None)) {
                Some(CatenaryAction::Deny(msg)) => return Some(HookResult::Deny(msg)),
                Some(
                    CatenaryAction::EditingStart
                    | CatenaryAction::Diagnostics
                    | CatenaryAction::Claim
                    | CatenaryAction::Allow { .. },
                ) => return None,
                Some(CatenaryAction::NotCatenary) | None => {}
            }
        }

        Some(HookResult::Deny(self.boundary_block_message(
            session_id, agent_id, command, tool_name,
        )))
    }

    /// Build the editing-gate message — a helpful next step, not a fault.
    ///
    /// Lists the files edited-but-not-yet-diagnosed, grouped under each
    /// diagnostic feeder (LSP server / linter) tracking them, then teaches the
    /// two ways to clear them: bare `catenary diagnostics` (all) and
    /// `catenary diagnostics <those files>` (scoped, shown with the agent's real
    /// outstanding paths). The blocked command is named only to close the loop
    /// ("then re-run it"). It carries no inferred intent
    /// ("before testing"/"before building" are guesses and often wrong) and no
    /// Catenary-internals vocabulary — it anchors only on what the agent needs:
    /// these edited files haven't been diagnosed yet, here's the command.
    fn boundary_block_message(
        &self,
        session_id: Option<&str>,
        agent_id: &str,
        command: Option<&str>,
        tool_name: &str,
    ) -> String {
        use std::collections::BTreeMap;
        use std::fmt::Write as _;

        // The gate fires on undelivered debt, so the message lists exactly the
        // undelivered files — the ones that "haven't been diagnosed yet"
        // (misc 141); already-delivered batch files are left out.
        let files = self.session.editing.undelivered_files(session_id, agent_id);

        // Group the outstanding files under each feeder tracking them. A file
        // both a language server and a linter cover appears under both — the
        // agent sees every tool that will check it. `BTreeMap` gives a stable
        // (alphabetical) feeder order. A file with no resolvable feeder still
        // appears (via `unattributed`) so nothing silently drops off the debt.
        let mut by_feeder: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut unattributed: Vec<String> = Vec::new();
        for file in &files {
            let shown = file.display().to_string();
            let feeders = self.session.diagnostic_feeders(file);
            if feeders.is_empty() {
                unattributed.push(shown);
            } else {
                for feeder in feeders {
                    by_feeder.entry(feeder).or_default().push(shown.clone());
                }
            }
        }

        let mut msg = String::from("These files were edited but haven't been diagnosed yet:\n");
        for (feeder, group) in &by_feeder {
            let _ = writeln!(msg, "  {feeder}");
            for file in group {
                let _ = writeln!(msg, "    {file}");
            }
        }
        for file in &unattributed {
            let _ = writeln!(msg, "  {file}");
        }

        let scoped = files
            .iter()
            .map(|f| f.display().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let _ = writeln!(
            msg,
            "Run `catenary diagnostics` to check them all, or name paths to check only some:"
        );
        let _ = writeln!(msg, "  catenary diagnostics {scoped}");
        let what = command.unwrap_or(tool_name);
        let _ = write!(msg, "Then re-run `{what}`.");
        msg
    }

    /// Acquires the cross-session editing guardrail for `file_path`'s root.
    ///
    /// Returns `Some(Deny)` with guidance when another session holds the
    /// lock on that root; otherwise `None` (lock acquired or re-affirmed
    /// for this session, or there is no guardrail / no resolvable root).
    /// Locks are acquired lazily per-root, so only roots with actual edits
    /// are locked.
    /// `file_path` must already be canonicalized by the caller (roots are
    /// canonical, so `resolve_root` needs a canonical path to match) — see
    /// [`Self::handle_enforce_editing`].
    fn acquire_editing_guardrail(&self, file_path: Option<&Path>) -> Option<HookResult> {
        if let Some(guardrail) = &self.session.editing_guardrail
            && let Some(root) = file_path.and_then(|p| self.session.resolve_root(p))
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
        if !tool_name.is_some_and(is_edit_tool) {
            return None;
        }

        // Only accumulate files a diagnostic feeder covers whose root has not
        // suppressed the diagnostics surface — files without coverage (or in a
        // `disable_diag` root) have nothing to report in done_editing.
        //
        // Canonicalize the edit path (when it exists) BEFORE the coverage check:
        // roots are canonical (the daemon canonicalizes `CATENARY_ROOTS` and every
        // ephemeral mount), but the host passes the file path verbatim. On a
        // symlinked-tempdir host (macOS `$TMPDIR` → `/private/var/…`, or any
        // symlinked prefix) the two spellings differ, so the un-canonicalized path
        // would fail the roots prefix check and a genuinely covered edit would be
        // mis-filed as out-of-root. Canonicalizing here compares canonical-to-
        // canonical and stores the canonical path in the batch, matching the
        // convention `ensure_ephemeral_mounts` already follows. A not-yet-existing
        // path keeps its spelling (canonicalize can't resolve it).
        let raw = Path::new(file_path);
        let owned = raw.canonicalize().unwrap_or_else(|_| raw.to_path_buf());
        let path = owned.as_path();
        if self.session.covered_for_diagnostics(path) {
            // A covered edit entered editing mode in `handle_enforce_editing`
            // (an allowed, non-guardrail-denied covered edit always does), so
            // the entry exists — accumulate the file into it.
            if self.session.editing.is_editing(session_id, agent_id) {
                self.record_covered_write(path, session_id, agent_id, "edited");
            }
        } else if self.session.is_within_roots(path) {
            if self.session.has_unverified_only_coverage(path) {
                // In-root, covered ONLY by an unverified (enrichment-only) server
                // (diagnostics-debt 04b): a diagnostics server exists but is not
                // blessed, so Catenary withholds its diagnostics and the gate
                // does not arm. Its own bucket, distinct from the truly-uncovered
                // one, so the note declares "not diagnostics-covered" (a server
                // exists) rather than "no covering server" (none does) — the
                // footgun ruling: unknowns get declaration, not interpretation.
                self.session.editing.record_unverified_edit(
                    session_id,
                    agent_id,
                    skipped_display_name(path),
                );
                debug!(
                    source = Source::HookDispatch.as_str(),
                    file = file_path,
                    "file skipped (in-root, only an unverified server covers it)",
                );
            } else {
                // In-root but no covering feeder (misc 173): a different predicate
                // from out-of-roots — the root IS tracked; the file's type just has
                // no configured server or linter (`Makefile`, `.txt`, logs).
                // Recorded in its own bucket with the file's display name so the
                // bare-run note names what went unchecked instead of claiming the
                // edit was outside a root it then prints. Same standalone
                // semantics as the outside leg: the entry is created when needed,
                // holds no files, and never trips the gate.
                self.session.editing.record_uncovered_edit(
                    session_id,
                    agent_id,
                    skipped_display_name(path),
                );
                debug!(
                    source = Source::HookDispatch.as_str(),
                    file = file_path,
                    "file skipped (in-root, no covering feeder)",
                );
            }
        } else {
            // Out-of-root edit: `handle_enforce_editing` let it flow
            // free WITHOUT entering editing mode, so a *standalone* one (no
            // covered edit alongside it) leaves no editing entry for the old
            // increment to bump — that gap is why a bare `catenary
            // diagnostics` after only out-of-root edits lied with
            // `[no edited files]` (bug 58 / ephemeral-roots ticket 01).
            // `record_outside_edit` creates the entry when needed and carries
            // the enclosing project root (walk repository markers up) for the
            // root-aware note. The entry holds no files, so it never trips the gate and is
            // cleared silently at stop if never diagnosed.
            let root = crate::companions::enclosing_worktree_root(path);
            self.session
                .editing
                .record_outside_edit(session_id, agent_id, root);
            debug!(
                source = Source::HookDispatch.as_str(),
                file = file_path,
                "file skipped (outside tracked roots)",
            );
        }
        None
    }

    /// Record one covered write into the caller's modified-set and surface it
    /// on the session board as `<verb> <path>`.
    ///
    /// The shared core of the Edit/Write path
    /// ([`Self::handle_file_accumulation`], `verb = "edited"`) and the
    /// resolved-shell-write path ([`Self::handle_shell_write_accumulation`],
    /// `verb = "wrote"`), so both attribute identically. The first covered
    /// write of an editing batch also flips the session-board status to
    /// `editing` — `set_last_action` marks the snapshot dirty (ticket 05).
    ///
    /// `path` must already be canonicalized by the caller. Existence is probed
    /// here (record time) to seed the phantom-vs-real latch (bug 76): a write
    /// resolved and recorded at `PreToolUse` whose command then fails wholesale
    /// leaves a target that never comes to exist, and the batch must not gate on
    /// it. A target that already exists is a write/edit to a real file; a
    /// not-yet-existing one is a to-be-created target, reconciled against disk
    /// at the next boundary.
    fn record_covered_write(
        &self,
        path: &Path,
        session_id: Option<&str>,
        agent_id: &str,
        verb: &str,
    ) {
        let existed_at_record = path.exists();
        self.session.editing.record_covered_edit(
            session_id,
            agent_id,
            path.to_path_buf(),
            existed_at_record,
        );
        self.session
            .set_last_action(format!("{verb} {}", self.session.display_path(path)));
        debug!(
            source = Source::HookDispatch.as_str(),
            file = %path.display(),
            "file recorded in diagnostics batch",
        );
    }

    /// Accumulates the write-set the command filter resolved from a shell
    /// command into the caller's modified-set.
    ///
    /// The shell-write twin of [`Self::handle_file_accumulation`] (ws38
    /// ticket 02, decision 026): the writes were resolved and opaque-gated
    /// client-side at `PreToolUse` — before the command runs — and carried
    /// here on the allowed request. Covered targets (a diagnostic feeder covers
    /// them and their root has not set `disable_diag`) enter the tracked set
    /// under `(session_id, agent_id)`, the **first** covered write entering
    /// editing mode implicitly — exactly like the first edit; uncovered targets
    /// (`> hits.txt` with no feeder) record nothing and never gate.
    ///
    /// Recording happens before execution, so a command that then fails
    /// wholesale would leave phantom targets that never came to exist. Each
    /// recorded target carries its record-time disk existence (bug 76): a
    /// never-materialized target is dropped at the next boundary block or
    /// diagnose snapshot ([`EditingManager::reconcile`]), so a failed write
    /// arms no gate and prints no receipt line, while a real edit — written and
    /// perhaps later deleted — survives to be reported honestly.
    fn handle_shell_write_accumulation(
        &self,
        writes: &[PathBuf],
        session_id: Option<&str>,
        agent_id: &str,
    ) {
        let mut started = self.session.editing.is_editing(session_id, agent_id);
        let mut outside = 0usize;
        let mut uncovered: Vec<String> = Vec::new();
        let mut unverified: Vec<String> = Vec::new();
        for path in writes {
            // Canonicalize (when it exists) before the coverage check so a
            // symlinked prefix does not mis-file a covered write as out-of-root —
            // the same canonical-to-canonical alignment `handle_file_accumulation`
            // applies. A not-yet-created write target keeps its spelling.
            let path = path.canonicalize().unwrap_or_else(|_| path.clone());
            if self.session.covered_for_diagnostics(&path) {
                if !started {
                    let _ = self.session.editing.start_editing(session_id, agent_id);
                    started = true;
                }
                self.record_covered_write(&path, session_id, agent_id, "wrote");
            } else if self.session.is_within_roots(&path) {
                if self.session.has_unverified_only_coverage(&path) {
                    // In-root, only an unverified (enrichment-only) server covers
                    // it (diagnostics-debt 04b) — its own bucket, so the note
                    // declares "not diagnostics-covered" (a server exists) rather
                    // than "no covering server".
                    unverified.push(skipped_display_name(&path));
                } else {
                    // In-root, no covering feeder (misc 173) — its own bucket,
                    // named, so the note doesn't misattribute it to root coverage.
                    uncovered.push(skipped_display_name(&path));
                }
            } else {
                outside += 1;
            }
        }
        // The skip records are no-ops until the agent's editing entry exists,
        // so buffer them and apply only once a covered write has started the
        // entry — independent of write ordering. A command whose targets are
        // all skipped starts no entry and reports no skipped counts: there is
        // nothing to drain for this agent.
        if started {
            for _ in 0..outside {
                self.session.editing.increment_outside(session_id, agent_id);
            }
            for name in uncovered {
                self.session
                    .editing
                    .record_uncovered_edit(session_id, agent_id, name);
            }
            for name in unverified {
                self.session
                    .editing
                    .record_unverified_edit(session_id, agent_id, name);
            }
        }
    }

    /// Transfer a landed worktree's **unpaid** diagnostics debt to the landing
    /// agent (misc 189) — the ruled land/ledger seam.
    ///
    /// Landing a worktree takes on its content, and — per the maintainer's ruling
    /// — its debt: exactly the files the worker left **unpaid** (never ran
    /// `catenary diagnostics` over), and nothing more. Debt follows unpaid
    /// content and *transfers*; it never duplicates. This replaces the retired
    /// content-based arm (which armed the whole landed diff regardless of
    /// payment, and split inconsistently at larger file counts — misc 189's dig).
    ///
    /// Reads the worktree's sidecar for the owner's `(session_id, source_repo)`,
    /// derives the owner's `agent_id` from the worktree's leaf directory name
    /// ([`crate::worktree_land::worktree_owner_label`], bug 91), and looks the
    /// owner's still-undelivered batch up in this session's ledger. The batch is
    /// keyed `(session_id, agent_id)` and lives with the daemon instance — so a
    /// worktree whose worker **paid**, whose worker **never edited**, or whose
    /// batch **died with a bounced daemon** (bug 79) all present the same
    /// debt-free ledger and arm nothing (the never-lock-out doctrine).
    ///
    /// `landed` is the land's resolved write-set (the applied files, mapped onto
    /// the owning repo). The transfer is the owner-unpaid set intersected with
    /// what actually landed ([`crate::worktree_land::owner_unpaid_landed`]): a
    /// file the owner never paid but that did not land (a conflict) transfers no
    /// debt. Each transferred path is recorded into the landing agent's batch
    /// exactly like a covered edit, the first one entering editing mode.
    ///
    /// A missing/unparseable sidecar, or a non-covered owner session, arms
    /// nothing — a silent no-op, never a lock-out.
    fn handle_worktree_land_debt_transfer(
        &self,
        land_path: &str,
        landed: &[PathBuf],
        session_id: Option<&str>,
        agent_id: &str,
    ) {
        // Canonicalize the worktree path the same way the daemon land handler
        // does, so the owner's canonical batch paths strip-prefix cleanly.
        let worktree = Path::new(land_path)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(land_path));

        // The owner's identity: session + owning repo from the sidecar, agent id
        // from the worktree's leaf directory name.
        let sidecar = crate::worktree_create::sidecar_path(&worktree);
        let Some(meta) = std::fs::read_to_string(&sidecar)
            .ok()
            .and_then(|c| serde_json::from_str::<crate::worktree_create::WorktreeMeta>(&c).ok())
        else {
            return; // not a registered worktree — nothing to transfer
        };
        let owner_label = crate::worktree_land::worktree_owner_label(&worktree);

        // The owner's still-unpaid batch (debt-free if empty — paid, un-edited, or
        // died with a bounced daemon). Keyed on the owner's recorded session.
        let owner_session = (!meta.session_id.is_empty()).then_some(meta.session_id.as_str());
        let owner_unpaid = self
            .session
            .editing
            .undelivered_files(owner_session, &owner_label);
        if owner_unpaid.is_empty() {
            return;
        }

        // Transfer exactly the unpaid files that actually landed, mapped onto the
        // owning repo.
        let landed_set: std::collections::BTreeSet<PathBuf> = landed.iter().cloned().collect();
        let transfer = crate::worktree_land::owner_unpaid_landed(
            &owner_unpaid,
            &worktree,
            &meta.source_repo,
            &landed_set,
        );

        let mut started = self.session.editing.is_editing(session_id, agent_id);
        for path in &transfer {
            // Canonicalize (when it exists) before the coverage check and record,
            // matching `handle_shell_write_accumulation`'s canonical alignment.
            let path = path.canonicalize().unwrap_or_else(|_| path.clone());
            if self.session.covered_for_diagnostics(&path) {
                if !started {
                    let _ = self.session.editing.start_editing(session_id, agent_id);
                    started = true;
                }
                self.record_covered_write(&path, session_id, agent_id, "landed");
            }
        }
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
            if self.session.editing.has_undelivered(session_id, agent_id) {
                Some(HookResult::Block(
                    "run `catenary diagnostics` before finishing".into(),
                ))
            } else {
                // No undelivered debt (nothing edited, or the batch is fully
                // diagnosed) — silently clear editing state.
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
        // Antigravity CLI edit tools
        assert!(is_edit_tool("write_to_file"));
        assert!(is_edit_tool("replace_file_content"));
        assert!(is_edit_tool("multi_replace_file_content"));
        // OpenCode edit tools (lowercase built-ins)
        assert!(is_edit_tool("edit"));
        assert!(is_edit_tool("write"));
        assert!(is_edit_tool("patch"));
        // Non-edit tools
        assert!(!is_edit_tool("Read"));
        assert!(!is_edit_tool("Bash"));
        assert!(!is_edit_tool("grep"));
        assert!(!is_edit_tool("read"));
    }

    #[test]
    fn test_is_read_tool() {
        assert!(is_read_tool("Read"));
        assert!(is_read_tool("NotebookRead"));
        assert!(is_read_tool("read_file"));
        // OpenCode read tool (lowercase built-in)
        assert!(is_read_tool("read"));
        assert!(!is_read_tool("Edit"));
        assert!(!is_read_tool("Bash"));
        assert!(!is_read_tool("run_command"));
        assert!(!is_read_tool("edit"));
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
            .record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);

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
            .record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);

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
            reason.contains("make test") && reason.contains("catenary diagnostics"),
            "foreign cmd → boundary block names the command and the fix, got: {reason}"
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

        let files = router.session.editing.files(None, "");
        assert_eq!(files, vec![PathBuf::from(&main_rs)]);
    }

    #[test]
    fn test_hook_require_release_block() {
        let router = test_router();
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);

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
            writes: Vec::new(),
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

        let files = router.session.editing.files(None, "");
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
            writes: Vec::new(),
        });
        let r2 = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(f2.clone()),
            command: None,
            agent_id: String::new(),
            session_id: None,
            writes: Vec::new(),
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
        let files = router.session.editing.files(None, "");
        assert_eq!(files, vec![PathBuf::from(&f1), PathBuf::from(&f2)]);
        assert_eq!(
            router.session.editing.clear_all(),
            1,
            "mode entered exactly once"
        );
    }

    #[test]
    fn uncovered_standalone_edit_counts_filtered_but_never_gates() {
        // Bug 58 regression: no roots → no LSP coverage for the edited file, and
        // no covered edit alongside it to open the editing entry. The edit is
        // allowed (never gates), accumulates NO covered file — but it IS counted
        // as filtered now, so a later bare `catenary diagnostics` surfaces it
        // instead of the bare `[no edited files]` lie. (Before the fix this
        // standalone out-of-root edit vanished: filtered stayed 0.)
        let router = test_router();
        let res = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some("/outside/some/file.rs".to_string()),
            command: None,
            agent_id: String::new(),
            session_id: None,
            writes: Vec::new(),
        });
        assert!(
            res.result.is_none(),
            "uncovered edit allowed, got {:?}",
            res.result
        );
        // The filtered-only entry holds no covered files, so the boundary block
        // (which gates on a non-empty covered set) never fires: the edit still
        // never gates unrelated commands.
        assert!(
            !router.session.editing.has_files(None, ""),
            "uncovered edit accumulates no covered file, so it never gates"
        );
        assert!(
            router.session.editing.files(None, "").is_empty(),
            "uncovered file not accumulated"
        );
        assert_eq!(
            router.session.editing.skipped(None, "").outside,
            1,
            "the standalone out-of-root edit is counted as outside (bug 58)"
        );
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
        // but should land in the outside bucket.
        router.handle_file_accumulation("/outside/some/file.rs", None, "", Some("Edit"));
        let files = router.session.editing.files(None, "");
        assert!(
            files.is_empty(),
            "out-of-root file should not be accumulated"
        );
        let skipped = router.session.editing.skipped(None, "");
        assert_eq!(
            skipped.outside, 1,
            "out-of-root edit should count as outside"
        );
        assert_eq!(
            skipped.uncovered, 0,
            "out-of-root edit must not land in the in-root uncovered bucket"
        );
    }

    #[test]
    fn test_file_accumulation_keeps_in_root() {
        let (router, root) = test_router_with_root();
        let _ = router.session.editing.start_editing(None, "");

        let in_root = format!("{}/src/main.rs", root.display());
        router.handle_file_accumulation(&in_root, None, "", Some("Edit"));
        let files = router.session.editing.files(None, "");
        assert_eq!(files.len(), 1, "in-root file should be accumulated");
    }

    // ── Single-file cache scope boundary tests ─────────────────────────

    /// Fake language ID matching the manager tests. Files with extension
    /// `.yX4Za` resolve to this via the raw-extension fallback in
    /// `language_id()`.
    const SF_LANG: &str = "yX4Za";
    // The `mockls-event` persona (blessed, event discipline; diagnostics-debt
    // 04c) so the single-file server counts as diagnostics coverage — the
    // editing-entry/coverage tests below assert on the blessed set. The language
    // stays `yX4Za`; only the server key is the persona.
    const SF_SERVER: &str = "mockls-event";

    /// Build a config with a single language+server for single-file
    /// cache tests. No real LSP binary needed — these tests only check
    /// cache-driven routing in the hook layer.
    fn sf_test_config() -> Config {
        use crate::config::{LanguageConfig, ServerBinding, ServerDef};

        let mut config = Config::default();
        config.server.insert(
            SF_SERVER.to_string(),
            ServerDef {
                path: Some("mockls".to_string()),
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
        let dir = tempfile::tempdir().expect("tempdir");

        let config = sf_test_config();
        let logging = crate::logging::LoggingServer::new();
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let instance_id: Arc<str> = "test-session".into();
        let parent_context = crate::bridge::parent_context::ParentContextQueue::new();
        let session = Arc::new(Session::new(
            config,
            vec![],
            logging,
            instance_id.clone(),
            handle,
            parent_context,
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

        let router = HookRouter::new(session, instance_id, "test".to_string());

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
        let files = router.session.editing.files(None, "");
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
        let files = router.session.editing.files(None, "");
        assert!(
            files.is_empty(),
            "runtime-failed out-of-root file should not be accumulated"
        );
    }

    // ── Filesystem Bash allowlist tests ──────────────────────────────────

    #[test]
    fn test_is_bash_tool() {
        assert!(is_bash_tool("Bash"));
        assert!(is_bash_tool("run_command")); // Antigravity CLI
        assert!(is_bash_tool("bash")); // OpenCode (lowercase built-in)
        assert!(!is_bash_tool("Edit"));
        assert!(!is_bash_tool("Read"));
        assert!(!is_bash_tool("BASH")); // case-sensitive
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
            .record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);

        // Filesystem-only Bash — should allow during editing
        let result = router.handle_enforce_editing("Bash", None, Some("rm -rf target/"), None, "");
        assert!(
            result.is_none(),
            "filesystem-only Bash should be allowed during editing, got {result:?}"
        );

        // Antigravity CLI shell tool with filesystem command
        let result = router.handle_enforce_editing(
            "run_command",
            None,
            Some("mkdir -p src/new_module"),
            None,
            "",
        );
        assert!(
            result.is_none(),
            "filesystem-only run_command should be allowed, got {result:?}"
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
            .record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);

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
            .record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);

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
        // The Edit targets a real file — Edit acts on an existing file, and the
        // reconcile step (bug 76) keeps only edits that materialized on disk.
        std::fs::create_dir_all(format!("{}/src", root.display())).expect("mkdir src");
        std::fs::write(&in_root, "fn main() {}\n").expect("write edit target");

        // A covered edit (dispatched end-to-end) enters editing mode and
        // accumulates the file.
        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(in_root.clone()),
            command: None,
            agent_id: String::new(),
            session_id: None,
            writes: Vec::new(),
        });
        assert!(router.session.editing.has_files(None, ""));

        // A non-edit command is now blocked; the message names the command,
        // lists the tracked file, and points at `catenary diagnostics`.
        let result = router.handle_enforce_editing("Bash", None, Some("make test"), None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("expected boundary block, got {result:?}");
        };
        assert!(
            reason.contains("make test"),
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
            writes: Vec::new(),
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
            writes: Vec::new(),
        });
        assert!(router.session.editing.has_files(None, ""));

        // Canonical Catenary commands stay allowed mid-editing even with a
        // covered set pending: search and the boundary diagnostics command.
        for cmd in [
            "catenary grep needle",
            "catenary glob foo.rs",
            "catenary diagnostics",
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
            writes: Vec::new(),
        });
        assert!(
            router.session.editing.has_files(None, ""),
            "covered in-root edit accumulated despite cold per-root instance"
        );
    }

    // ── Gate message: feeder grouping (ticket 03) ───────────────────────

    #[test]
    fn gate_message_groups_files_by_lsp_feeder() {
        // Two served .rs files → both grouped under the language server, both
        // command forms shown, the scoped one with the real paths.
        let (router, root) = test_router_with_root();
        let main_rs = root.join("src/main.rs");
        let lib_rs = root.join("src/lib.rs");
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .record_covered_edit(None, "", main_rs.clone(), true);
        router
            .session
            .editing
            .record_covered_edit(None, "", lib_rs.clone(), true);

        let msg = router.boundary_block_message(None, "", Some("make test"), "Bash");

        assert!(
            msg.contains("rust-analyzer"),
            "names the tracking LSP feeder: {msg}"
        );
        assert!(
            msg.contains(&main_rs.display().to_string()),
            "lists main.rs: {msg}"
        );
        assert!(
            msg.contains(&lib_rs.display().to_string()),
            "lists lib.rs: {msg}"
        );
        // Bare form (all) and scoped form (the agent's real files).
        assert!(
            msg.contains("Run `catenary diagnostics` to check them all"),
            "teaches the bare form: {msg}"
        );
        assert!(
            msg.contains(&format!(
                "catenary diagnostics {} {}",
                main_rs.display(),
                lib_rs.display()
            )),
            "teaches the scoped form with the real files: {msg}"
        );
        // Helpful next step, names the blocked command to re-run.
        assert!(
            msg.contains("make test"),
            "closes the loop on the blocked command: {msg}"
        );
        // No fault framing, no Catenary-internals vocabulary.
        assert!(!msg.contains("is blocked"), "not a fault message: {msg}");
        for banned in ["handoff", "consume", "guardrail"] {
            assert!(
                !msg.contains(banned),
                "no internals vocab ({banned}): {msg}"
            );
        }
    }

    #[test]
    fn gate_message_groups_files_by_linter_feeder() {
        // A .txt file has no language server; a linter rule covers it, so the
        // gate tracks it and the message attributes it to the linter alone.
        let (router, root) = test_router_with_project_config(
            "[linter.rule.mocklint]\ncommand = \"mocklint\"\npatterns = [\"**/*.txt\"]\n",
        );
        let notes = root.join("notes.txt");
        assert!(
            router.session.covered_for_diagnostics(&notes),
            "lint-covered .txt is gated for diagnostics"
        );
        assert_eq!(
            router.session.diagnostic_feeders(&notes),
            vec!["mocklint".to_string()],
            "linter-only feeder attribution"
        );

        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .record_covered_edit(None, "", notes.clone(), true);
        let msg = router.boundary_block_message(None, "", Some("cargo build"), "Bash");

        assert!(msg.contains("mocklint"), "names the linter feeder: {msg}");
        assert!(
            msg.contains(&notes.display().to_string()),
            "lists the tracked file: {msg}"
        );
        assert!(
            !msg.contains("rust-analyzer"),
            "no phantom LSP feeder for a lint-only file: {msg}"
        );
        assert!(
            msg.contains(&format!("catenary diagnostics {}", notes.display())),
            "scoped form names the file: {msg}"
        );
    }

    #[test]
    fn gate_message_groups_files_across_mixed_feeders() {
        // A linter rule that matches both .rs and .txt: the served .rs file is
        // tracked by rust-analyzer AND the linter (listed under both); the .txt
        // by the linter only. The scoped form still names each file once.
        let (router, root) = test_router_with_project_config(
            "[linter.rule.mocklint]\ncommand = \"mocklint\"\npatterns = [\"**/*.rs\", \"**/*.txt\"]\n",
        );
        let main_rs = root.join("src/main.rs");
        let notes = root.join("notes.txt");

        assert_eq!(
            router.session.diagnostic_feeders(&main_rs),
            vec!["mocklint".to_string(), "rust-analyzer".to_string()],
            "a mixed file lists both feeders, alphabetically"
        );
        assert_eq!(
            router.session.diagnostic_feeders(&notes),
            vec!["mocklint".to_string()],
            "a lint-only file lists just the linter"
        );

        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .record_covered_edit(None, "", main_rs.clone(), true);
        router
            .session
            .editing
            .record_covered_edit(None, "", notes.clone(), true);
        let msg = router.boundary_block_message(None, "", Some("make check"), "Bash");

        assert!(msg.contains("rust-analyzer"), "names the LSP feeder: {msg}");
        assert!(msg.contains("mocklint"), "names the linter feeder: {msg}");
        assert!(
            msg.contains(&main_rs.display().to_string()),
            "lists the mixed file: {msg}"
        );
        assert!(
            msg.contains(&notes.display().to_string()),
            "lists the lint-only file: {msg}"
        );
        // The scoped command dedups to one entry per file, in edit order.
        assert!(
            msg.contains(&format!(
                "catenary diagnostics {} {}",
                main_rs.display(),
                notes.display()
            )),
            "scoped form names each outstanding file once: {msg}"
        );
    }

    // ── Test helpers ────────────────────────────────────────────────────

    /// Create a `HookRouter` with minimal dependencies for handler unit tests.
    ///
    /// Uses minimal dependencies (no live LSP servers). Editing state is
    /// managed in-memory via [`super::super::editing_manager::EditingManager`]
    /// on the `Session`.
    fn test_router() -> TestHookRouter {
        let dir = tempfile::tempdir().expect("tempdir");

        let config = Config::default();
        let logging = crate::logging::LoggingServer::new();

        // Session requires a tokio runtime handle for async dispatch.
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let instance_id: Arc<str> = "test-session".into();
        let parent_context = crate::bridge::parent_context::ParentContextQueue::new();
        let session = Arc::new(Session::new(
            config,
            vec![],
            logging,
            instance_id.clone(),
            handle,
            parent_context,
            None,
        ));
        let router = HookRouter::new(session, instance_id, "test".to_string());

        TestHookRouter {
            _dir: dir,
            _runtime: runtime,
            router,
        }
    }

    /// Create a `HookRouter` with a workspace root for scope boundary tests.
    ///
    /// Loads the embedded default classification + server bindings
    /// (`default_with_classification`) so in-root coverage gating sees the
    /// real served/unserved split: `.rs` resolves to a configured
    /// rust-analyzer binding (served), while types with no `servers` entry
    /// (e.g. `.txt`, logs) are unserved.
    fn test_router_with_root() -> (TestHookRouter, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");

        let config = Config::default_with_classification();
        let logging = crate::logging::LoggingServer::new();

        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        // Canonicalize the tempdir: production roots arrive canonicalized (the
        // tracker canonicalizes every root), and the dispatch paths canonicalize
        // file paths before the coverage check (`handle_file_accumulation`'s
        // canonical-to-canonical alignment). A raw fixture root splits that
        // alignment wherever the tempdir rides a symlink — macOS's
        // /var → /private/var made these tests CI-red while Linux stayed green
        // (reproducible on Linux with a symlinked TMPDIR).
        let root = dir
            .path()
            .canonicalize()
            .expect("canonical tempdir")
            .join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");

        let instance_id: Arc<str> = "test-session".into();
        let parent_context = crate::bridge::parent_context::ParentContextQueue::new();
        let session = Arc::new(Session::new(
            config,
            vec![root.clone()],
            logging,
            instance_id.clone(),
            handle,
            parent_context,
            None,
        ));

        let router = HookRouter::new(session, instance_id, "test".to_string());

        (
            TestHookRouter {
                _dir: dir,
                _runtime: runtime,
                router,
            },
            root,
        )
    }

    /// Create a `HookRouter` rooted at a fresh workspace whose `.catenary.toml`
    /// is seeded with `project_config` before the session loads it.
    ///
    /// `Root::load` reads the project config at birth, so a `[linter.rule.*]`
    /// entry written here registers a standalone-linter feeder — used to
    /// exercise the gate message's linter-only and mixed-feeder grouping.
    fn test_router_with_project_config(project_config: &str) -> (TestHookRouter, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");
        std::fs::write(root.join(".catenary.toml"), project_config).expect("write project config");

        let config = Config::default_with_classification();
        let logging = crate::logging::LoggingServer::new();
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let instance_id: Arc<str> = "test-session".into();
        let parent_context = crate::bridge::parent_context::ParentContextQueue::new();
        let session = Arc::new(Session::new(
            config,
            vec![root.clone()],
            logging,
            instance_id.clone(),
            handle,
            parent_context,
            None,
        ));

        let router = HookRouter::new(session, instance_id, "test".to_string());

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
        let dir = tempfile::tempdir().expect("tempdir");

        // Load the embedded classification + server bindings so in-root `.rs`
        // edits are served (covered) — the cross-session deny path is gated on
        // coverage.
        let config = Config::default_with_classification();
        let logging = crate::logging::LoggingServer::new();

        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
        let handle = runtime.handle().clone();

        let root = dir.path().join("workspace");
        std::fs::create_dir_all(&root).expect("create workspace dir");

        let instance_id: Arc<str> = "test-session".into();
        let parent_context = crate::bridge::parent_context::ParentContextQueue::new();
        // The primary session owns the shared resources (fs_manager carries
        // the workspace root); the per-session daemon session carries the
        // guardrail under test.
        let primary = Session::new(
            config,
            vec![root.clone()],
            logging,
            instance_id.clone(),
            handle,
            parent_context,
            None,
        );
        let guardrail = Arc::new(crate::bridge::editing_guardrail::EditingGuardrail::new());
        let session = Arc::new(Session::new_for_daemon(
            &primary,
            instance_id.clone(),
            Some(guardrail.clone()),
        ));

        let router = HookRouter::new(session, instance_id, "test".to_string());

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

    // ── Parent-context delivery tests (misc 151) ──────────────────────

    #[test]
    fn pre_tool_allow_delivers_parent_context() {
        let router = test_router();
        router.session.parent_context.queue(
            "test-session",
            "subagent `a` left a dirty worktree".to_string(),
        );

        // An allowed PreToolUse by the parent (empty agent_id) drains it.
        let result = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Read".to_string(),
            file_path: None,
            command: None,
            agent_id: String::new(),
            session_id: None,
            writes: Vec::new(),
        });
        assert!(result.result.is_none(), "read allowed");
        assert_eq!(
            result.additional_context.as_deref(),
            Some("subagent `a` left a dirty worktree"),
        );
        // Drained — not redelivered on the next call.
        assert_eq!(router.session.parent_context.queue_len("test-session"), 0);
    }

    #[test]
    fn subagent_pre_tool_does_not_absorb_parent_context() {
        let router = test_router();
        router
            .session
            .parent_context
            .queue("test-session", "parent notice".to_string());

        // A subagent (non-empty agent_id) must not drain the parent's notice.
        let result = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Read".to_string(),
            file_path: None,
            command: None,
            agent_id: "sub-agent".to_string(),
            session_id: None,
            writes: Vec::new(),
        });
        assert!(result.additional_context.is_none());
        assert_eq!(
            router.session.parent_context.queue_len("test-session"),
            1,
            "notice preserved for the parent"
        );
    }

    #[test]
    fn stop_allow_delivers_parent_context() {
        let router = test_router();
        router
            .session
            .parent_context
            .queue("test-session", "dirty worktree kept".to_string());

        // Not editing → allow → drain on Stop.
        let result = router.dispatch(crate::hook::HookRequest::PostAgent {
            agent_id: String::new(),
            session_id: None,
            stop_hook_active: false,
        });
        assert!(result.result.is_none(), "should allow");
        assert_eq!(
            result.additional_context.as_deref(),
            Some("dirty worktree kept"),
        );
    }

    #[test]
    fn stop_block_preserves_parent_context() {
        let router = test_router();
        let _ = router.session.editing.start_editing(None, "");
        router
            .session
            .editing
            .record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);
        router
            .session
            .parent_context
            .queue("test-session", "dirty worktree kept".to_string());

        // The editing gate blocks the stop → no delivery, context preserved for
        // the next allowed response (the once-clean-turn rule, misc 151).
        let result = router.dispatch(crate::hook::HookRequest::PostAgent {
            agent_id: String::new(),
            session_id: None,
            stop_hook_active: false,
        });
        assert!(
            matches!(result.result, Some(HookResult::Block(_))),
            "should block"
        );
        assert!(
            result.additional_context.is_none(),
            "block delivers nothing"
        );
        assert_eq!(
            router.session.parent_context.queue_len("test-session"),
            1,
            "context preserved for the next allowed response"
        );
    }

    #[test]
    fn empty_parent_context_yields_no_additional_context() {
        let router = test_router();
        let result = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Read".to_string(),
            file_path: None,
            command: None,
            agent_id: String::new(),
            session_id: None,
            writes: Vec::new(),
        });
        assert!(result.additional_context.is_none());
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
            writes: Vec::new(),
        });

        let files = router.session.editing.files(None, "");
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
            writes: Vec::new(),
        });

        assert!(
            matches!(result.result, Some(HookResult::Deny(_))),
            "edit on a root owned by another session should be denied"
        );
        assert!(
            !router.session.editing.is_editing(None, ""),
            "denied first edit must not enter editing mode"
        );
        assert!(
            router.session.editing.files(None, "").is_empty(),
            "denied edit should not accumulate file"
        );
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
            writes: Vec::new(),
        });

        let files = router.session.editing.files(None, "");
        assert!(files.is_empty(), "non-edit tool should not accumulate file");
    }

    // ── Resolved shell-write accumulation (ws38 ticket 02) ─────────────

    #[test]
    fn dispatch_shell_write_covered_accumulates_and_enters_editing() {
        // A resolved shell write to a covered source file (e.g.
        // `catenary grep pat > src/main.rs`) creates the same debt an Edit
        // would: it enters editing mode implicitly and accumulates the target.
        let (router, root) = test_router_with_root();
        let target = PathBuf::from(format!("{}/src/main.rs", root.display()));
        assert!(!router.session.editing.is_editing(None, ""));

        let res = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Bash".to_string(),
            file_path: None,
            command: Some("catenary grep pat > src/main.rs".to_string()),
            agent_id: String::new(),
            session_id: None,
            writes: vec![target.clone()],
        });
        assert!(
            res.result.is_none(),
            "allowed shell write, got {:?}",
            res.result
        );
        assert!(
            router.session.editing.is_editing(None, ""),
            "first covered shell write enters editing mode"
        );
        let files = router.session.editing.files(None, "");
        assert_eq!(files, vec![target], "covered shell write accumulated");
    }

    #[test]
    fn dispatch_shell_write_uncovered_does_not_accumulate_or_gate() {
        // An uncovered artifact target (`catenary grep pat > hits.txt`, no
        // feeder) records nothing and never enters editing mode — it can never
        // gate.
        let (router, root) = test_router_with_root();
        let artifact = PathBuf::from(format!("{}/hits.txt", root.display()));
        assert!(
            !router.session.has_lsp_coverage(&artifact),
            "a .txt artifact has no LSP coverage"
        );

        let res = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Bash".to_string(),
            file_path: None,
            command: Some("catenary grep pat > hits.txt".to_string()),
            agent_id: String::new(),
            session_id: None,
            writes: vec![artifact],
        });
        assert!(res.result.is_none(), "allowed, got {:?}", res.result);
        assert!(
            !router.session.editing.is_editing(None, ""),
            "an uncovered shell write must not enter editing mode"
        );
        assert!(
            router.session.editing.files(None, "").is_empty(),
            "uncovered shell write not accumulated"
        );
    }

    #[test]
    fn dispatch_shell_write_empty_set_is_noop() {
        // A resolved command with no writes (e.g. a plain redirect to a sink,
        // a pure delete, or a read) carries an empty set and gates nothing.
        let (router, _root) = test_router_with_root();
        let res = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Bash".to_string(),
            file_path: None,
            command: Some("git status".to_string()),
            agent_id: String::new(),
            session_id: None,
            writes: Vec::new(),
        });
        assert!(res.result.is_none(), "allowed, got {:?}", res.result);
        assert!(
            !router.session.editing.is_editing(None, ""),
            "an empty write-set must not enter editing mode"
        );
    }

    #[test]
    fn dispatch_shell_write_mixed_covers_only_the_covered_target() {
        // A command writing both a covered source and an uncovered artifact
        // (`sed -i … src/main.rs && … > out.txt`) accumulates only the covered
        // target; the uncovered in-root one lands in the named uncovered
        // bucket (misc 173), and the debt still gates.
        let (router, root) = test_router_with_root();
        let covered = PathBuf::from(format!("{}/src/main.rs", root.display()));
        let uncovered = PathBuf::from(format!("{}/out.txt", root.display()));

        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Bash".to_string(),
            file_path: None,
            command: Some("sed -i s/a/b/ src/main.rs > out.txt".to_string()),
            agent_id: String::new(),
            session_id: None,
            writes: vec![covered.clone(), uncovered],
        });
        assert!(
            router.session.editing.has_files(None, ""),
            "the covered write arms the boundary block"
        );
        assert_eq!(
            router.session.editing.files(None, ""),
            vec![covered],
            "only the covered target accumulated"
        );
        let skipped = router.session.editing.skipped(None, "");
        assert_eq!(
            skipped.uncovered, 1,
            "the in-root uncovered target lands in the uncovered bucket"
        );
        assert!(
            skipped.uncovered_files.contains("out.txt"),
            "the uncovered target is recorded by name, got {:?}",
            skipped.uncovered_files
        );
        assert_eq!(
            skipped.outside, 0,
            "an in-root artifact is NOT outside tracked roots (misc 173)"
        );
    }

    #[test]
    fn dispatch_shell_write_denied_command_accumulates_nothing() {
        // A denied command never reaches this dispatch with a write-set — the
        // client rejects an opaque write before sending the request. The
        // daemon-side invariant: a denied tool call (`result` is `Some`)
        // accumulates nothing, even if writes were (defensively) present.
        let (router, root, guardrail) = test_router_with_guardrail();
        guardrail
            .try_acquire(&root, "other-session")
            .expect("foreign lock acquired");
        let target = PathBuf::from(format!("{}/src/main.rs", root.display()));

        // An Edit denied by the cross-session guardrail, carrying a stray
        // write-set: the deny short-circuits accumulation entirely.
        let result = router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Edit".to_string(),
            file_path: Some(format!("{}/src/main.rs", root.display())),
            command: None,
            agent_id: String::new(),
            session_id: None,
            writes: vec![target],
        });
        assert!(
            matches!(result.result, Some(HookResult::Deny(_))),
            "guardrail deny expected"
        );
        assert!(
            router.session.editing.files(None, "").is_empty(),
            "a denied call accumulates no writes"
        );
    }

    #[test]
    fn dispatch_shell_write_surfaces_last_action() {
        // The covered shell write surfaces on the session board as
        // "wrote <path>" — the faithful shell-write equivalent of the edit
        // path's "edited <path>".
        let (router, root) = test_router_with_root();
        let target = PathBuf::from(format!("{}/src/main.rs", root.display()));
        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Bash".to_string(),
            file_path: None,
            command: Some("cat tpl > src/main.rs".to_string()),
            agent_id: String::new(),
            session_id: None,
            writes: vec![target],
        });
        let action = router.session.last_action();
        assert!(
            action
                .as_ref()
                .is_some_and(|a| a.summary.starts_with("wrote ")),
            "covered shell write should surface as `wrote <path>`, got {action:?}",
        );
    }

    #[test]
    fn failed_shell_write_leaves_no_phantom_gate_debt() {
        // Bug 76 sighting: a resolved shell write to a covered target is
        // recorded at PreToolUse, before the command runs. The command then
        // failed wholesale (a `git apply` with wrong-cwd paths — zero bytes
        // written), so the target never came to exist. The next non-edit
        // command must NOT be gated on that phantom — the boundary block
        // reconciles the batch against disk first and drops it.
        let (router, root) = test_router_with_root();
        // A covered target that will NEVER be created on disk (the failed apply
        // resolved paths against the wrong repo).
        let phantom = PathBuf::from(format!("{}/src/phantom.rs", root.display()));

        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Bash".to_string(),
            file_path: None,
            command: Some("git apply tui09.diff".to_string()),
            agent_id: String::new(),
            session_id: None,
            writes: vec![phantom],
        });
        // The write was recorded (resolve-or-deny records before execution), so
        // pre-reconcile the batch holds the phantom and the gate looks armed.
        assert!(
            router.session.editing.has_undelivered(None, ""),
            "the write is recorded at PreToolUse — pre-reconcile the gate is armed"
        );

        // A subsequent non-edit command reaches the boundary block, which
        // reconciles first: the phantom target does not exist, so it is dropped
        // and the command flows free — no phantom gate debt.
        let result = router.handle_enforce_editing("Bash", None, Some("make test"), None, "");
        assert!(
            result.is_none(),
            "a failed write must not gate future work on a never-created file, got {result:?}"
        );
        assert!(
            router.session.editing.files(None, "").is_empty(),
            "the phantom entry is dropped from the batch on reconciliation"
        );
    }

    #[test]
    fn failed_shell_write_reconciled_but_real_edit_still_gates() {
        // The asymmetry the fix must preserve: a batch mixing a phantom (failed
        // write) and a real edit (a target that DID land on disk) drops only the
        // phantom. The real edit keeps the gate armed and appears in the block.
        let (router, root) = test_router_with_root();
        let phantom = PathBuf::from(format!("{}/src/phantom.rs", root.display()));
        let real = PathBuf::from(format!("{}/src/real.rs", root.display()));
        // Create only the real target on disk — the write to it succeeded.
        std::fs::create_dir_all(real.parent().expect("parent")).expect("mkdir");
        std::fs::write(&real, "fn real() {}\n").expect("write real target");

        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Bash".to_string(),
            file_path: None,
            command: Some("git apply patch.diff".to_string()),
            agent_id: String::new(),
            session_id: None,
            writes: vec![phantom, real.clone()],
        });

        // The boundary block reconciles: phantom dropped, real edit kept.
        let result = router.handle_enforce_editing("Bash", None, Some("make test"), None, "");
        let Some(HookResult::Deny(reason)) = result else {
            unreachable!("the surviving real edit must still gate, got {result:?}");
        };
        assert_eq!(
            router.session.editing.files(None, ""),
            vec![real],
            "only the real edit survives reconciliation"
        );
        assert!(
            reason.contains("real.rs") && !reason.contains("phantom.rs"),
            "the block lists the real edit, never the dropped phantom, got: {reason}"
        );
    }

    #[test]
    fn real_shell_write_then_deletion_is_reported_not_pruned() {
        // The honest-conservative boundary: a covered target that DID land on
        // disk (materialized at record time) and was later deleted must NOT be
        // pruned as a phantom — it was a genuine edit. It survives reconciliation
        // to keep the gate armed so `catenary diagnostics` reports it honestly.
        let (router, root) = test_router_with_root();
        let written = PathBuf::from(format!("{}/src/written.rs", root.display()));
        std::fs::create_dir_all(written.parent().expect("parent")).expect("mkdir");
        std::fs::write(&written, "fn written() {}\n").expect("write target");

        router.dispatch(crate::hook::HookRequest::PreTool {
            tool_name: "Bash".to_string(),
            file_path: None,
            command: Some("cat tpl > src/written.rs".to_string()),
            agent_id: String::new(),
            session_id: None,
            writes: vec![written.clone()],
        });
        assert!(
            router.session.editing.has_undelivered(None, ""),
            "the real write arms the gate"
        );

        // The agent then deletes the file before diagnosing it.
        std::fs::remove_file(&written).expect("delete the written file");

        // Reconciliation at the boundary must KEEP it — it was observed on disk
        // at record time (materialized), so its later absence is a real edit's
        // deletion, reported honestly, not a phantom to hide.
        let result = router.handle_enforce_editing("Bash", None, Some("make test"), None, "");
        assert!(
            matches!(result, Some(HookResult::Deny(_))),
            "a written-then-deleted real edit still gates, got {result:?}"
        );
        assert_eq!(
            router.session.editing.files(None, ""),
            vec![written],
            "the vanished real edit is retained for its honest receipt — not pruned"
        );
    }

    #[test]
    fn doc_only_in_root_edit_flows_free() {
        // Bug 44: an in-root file whose language has no configured server
        // (`.txt`, logs, data/scratch files) must NOT be treated as covered.
        // It flows free — not accumulated, filtered counter bumped — so the
        // editing boundary never sends the agent to run empty diagnostics.
        let (router, root) = test_router_with_root();
        let _ = router.session.editing.start_editing(None, "");

        let unserved = format!("{}/notes.txt", root.display());
        assert!(
            !router.session.has_lsp_coverage(Path::new(&unserved)),
            "in-root non-served type must not claim LSP coverage"
        );

        router.handle_file_accumulation(&unserved, None, "", Some("Edit"));
        let files = router.session.editing.files(None, "");
        assert!(
            files.is_empty(),
            "non-served in-root edit must not be accumulated"
        );
        let skipped = router.session.editing.skipped(None, "");
        assert_eq!(
            skipped.uncovered, 1,
            "non-served in-root edit lands in the uncovered bucket"
        );
        assert!(
            skipped.uncovered_files.contains("notes.txt"),
            "the uncovered file is recorded by name, got {:?}",
            skipped.uncovered_files
        );
        assert_eq!(
            skipped.outside, 0,
            "an in-root edit is NOT outside tracked roots (misc 173)"
        );
        router.session.editing.done_editing(None, "");

        // Contrast: a served in-root type (`.rs` → rust-analyzer) stays
        // covered and is accumulated, so diagnostics still flow for it.
        let _ = router.session.editing.start_editing(None, "");
        let served = format!("{}/src/main.rs", root.display());
        assert!(
            router.session.has_lsp_coverage(Path::new(&served)),
            "in-root served type must keep LSP coverage"
        );
        router.handle_file_accumulation(&served, None, "", Some("Edit"));
        let served_files = router.session.editing.files(None, "");
        assert_eq!(
            served_files.len(),
            1,
            "served in-root edit must still be accumulated"
        );
    }
}
