// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! In-memory editing state manager.
//!
//! Tracks which agents are in editing mode and accumulates modified file
//! paths during editing sessions. State is per-session lifetime — no
//! database persistence needed since the session owns the [`super::session::Session`]
//! which owns the `EditingManager`.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Result, anyhow};

/// Computes the editing state key from session and agent identifiers.
///
/// The key is `"{session_id}\0{agent_id}"` when a non-empty session ID is
/// present, falling back to just `agent_id` for backward compatibility
/// with hosts that don't provide a session ID.
///
/// `pub(crate)`: the held-open document registry tags each batch-opened
/// document with this same key as its owner (diagnostics-debt 01), so the
/// Stop/SubagentStop close resolves exactly the batch's identity.
pub(crate) fn editing_key(session_id: Option<&str>, agent_id: &str) -> String {
    match session_id {
        Some(sid) if !sid.is_empty() => format!("{sid}\0{agent_id}"),
        _ => agent_id.to_string(),
    }
}

/// One covered file in a batch, paired with whether a receipt covering it has
/// been handed to a client since its last recorded edit.
///
/// `delivered` is a *transport* fact, not a cleanliness one (misc 141): it flips
/// true only after the socket write of a `catenary diagnostics` response
/// succeeds — the daemon cannot know whether the agent actually read the bytes
/// (a pipe could rewrite them), only that they left. A fresh edit to the file
/// flips it back to false.
struct BatchFile {
    /// Absolute path of the covered file.
    path: PathBuf,
    /// Whether a receipt covering this file, computed after its last recorded
    /// edit, has been handed to a client (misc 141).
    delivered: bool,
    /// Whether this target has ever been observed to exist on disk — the
    /// phantom-vs-real latch (bug 76).
    ///
    /// A write set is resolved and recorded at `PreToolUse`, *before* the
    /// command runs (decision 026 resolve-or-deny). When the command then fails
    /// wholesale (the sighting: a `git apply` with wrong-cwd paths — exit
    /// non-zero, zero bytes written), the recorded targets never come to exist.
    /// Such a phantom entry must not arm the gate or print a
    /// `[path does not exist]` receipt line for a file nothing ever touched.
    ///
    /// `materialized` is set true at record time when the target already exists
    /// (a write/edit to a pre-existing file), and latched true by
    /// [`EditingManager::reconcile`] the first time a not-yet-materialized
    /// target is observed on disk (a successful new-file creation). It is the
    /// signal that separates *never-materialized* (drop silently — nonexistence
    /// of a never-created file is not a finding) from *written-then-deleted*
    /// (keep — the `[path does not exist]` receipt is the honest report of a
    /// vanished real edit). A file created *and* deleted entirely between two
    /// reconciliation passes — its creation never observed — is
    /// indistinguishable from a phantom with the records kept here and collapses
    /// into the phantom bucket; dropping it silently is the conservative choice.
    materialized: bool,
}

/// Per-agent editing accumulator: one **batch** plus the out-of-coverage note
/// metadata (misc 141).
///
/// The batch is the set of covered files this agent has edited, each carrying a
/// `delivered` flag. A covered edit into an *incomplete* batch (some flag false)
/// joins the file / flips its flag false; a covered edit into a *complete* batch
/// (non-empty, all flags true) discards the batch and starts a new one with that
/// file (the flat rule — no inside/outside distinction). Bare `catenary
/// diagnostics` diagnoses the whole batch and flips every flag; scoped
/// diagnostics flips exactly the named files. The debt gate is armed while any
/// flag is false. The batch is **in-memory** daemon state keyed by
/// `(session_id, agent_id)`: it persists across diagnose runs within a daemon
/// instance — diagnostics are always recomputed over it, so repeat bare runs
/// re-diagnose the same scope — and is **released with the instance**. On daemon
/// death the debt is dropped, never spooled (maintainer ruling, bug 79): a fresh
/// daemon disarms the gate so an unstable daemon cannot lock a session out.
///
/// Holds the covered batch alongside the [`SkippedEdits`] buckets — edits
/// skipped during accumulation because no diagnostic feeder covers them — so a
/// per-agent read reports the requesting agent's own skipped counts,
/// never another agent's (bug 37). Skipped edits carry no flag: they never
/// join the batch and never discard it (bug 58 note behavior unchanged).
#[derive(Default)]
struct EditingState {
    /// The current batch: covered files, each with a `delivered` flag.
    files: Vec<BatchFile>,
    /// Edits skipped during accumulation, split by predicate (misc 173).
    skipped: SkippedEdits,
}

/// Edits skipped during batch accumulation, split by the two distinct
/// predicates a receipt must not conflate (misc 173).
///
/// Root-containment and feeder-coverage are different facts: an in-root
/// `Makefile` has no covering server but is NOT outside tracked roots.
/// Rendering both buckets through one "outside tracked roots" line taught the
/// wrong lesson — the note named the containing root in the same parenthesis
/// that claimed the edit was outside it. The buckets ride the diagnostics
/// handoff and render as two distinct advisory lines on the bare-run receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkippedEdits {
    /// Edits made outside every tracked workspace root.
    pub outside: usize,
    /// Distinct enclosing project roots of the outside edits (walk repository
    /// markers up from each path), for the root-aware note (ephemeral-roots
    /// ticket 01 / bug 58). Empty when no outside edit had a detectable root.
    pub outside_roots: BTreeSet<PathBuf>,
    /// In-root edits no diagnostic feeder (server or linter) covers.
    pub uncovered: usize,
    /// Distinct display names (file names) of the uncovered in-root files, so
    /// the note can name what went unchecked ("(Makefile)").
    pub uncovered_files: BTreeSet<String>,
}

impl SkippedEdits {
    /// `true` when both buckets are empty — nothing skipped, no note to render.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.outside == 0 && self.uncovered == 0
    }
}

impl EditingState {
    /// Whether the batch is complete: non-empty and every file delivered.
    ///
    /// An empty batch (no covered files — e.g. a skipped-only entry) is *not*
    /// complete, so the next covered edit joins it rather than discarding it.
    fn is_complete(&self) -> bool {
        !self.files.is_empty() && self.files.iter().all(|f| f.delivered)
    }
}

/// In-memory editing state manager.
///
/// Owns editing state for a single Catenary session.
/// [`super::hook_router::HookRouter`] (which has the real `agent_id` from
/// the host CLI) accesses this through [`super::session::Session`].
///
/// State is keyed by a composite of `(session_id, agent_id)` to prevent
/// cross-session collisions when multiple host CLI sessions share a
/// workspace and route hooks to the same Catenary instance.
#[derive(Default)]
pub struct EditingManager {
    /// Active editing sessions: composite key → per-agent accumulator.
    state: Mutex<HashMap<String, EditingState>>,
}

impl EditingManager {
    /// Creates a new `EditingManager` with no active editing sessions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enters editing mode for an agent.
    ///
    /// # Errors
    ///
    /// Returns an error if the agent is already in editing mode.
    pub fn start_editing(&self, session_id: Option<&str>, agent_id: &str) -> Result<()> {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.contains_key(&key) {
            return Err(anyhow!("agent is already in editing mode"));
        }
        state.insert(key, EditingState::default());
        drop(state);
        Ok(())
    }

    /// Returns `true` if any agent in this session has an active editing
    /// accumulator.
    ///
    /// Session-wide (not keyed by agent): drives the snapshot session board's
    /// `editing` status (observability ticket 05). An accumulator is present
    /// from `start_editing` until `drain_all_and_clear` / `done_editing`, even
    /// before any file is added.
    #[must_use]
    pub fn is_active(&self) -> bool {
        !self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    }

    /// Returns `true` if **any** agent in this session has an undelivered
    /// covered file — the session-wide armed-gate signal (tui-rework 14, item 1).
    ///
    /// Distinct from [`is_active`](Self::is_active): a batch that has been fully
    /// diagnosed (`mark_delivered_all`) keeps its accumulator, so `is_active`
    /// stays `true` while this returns `false`. That gap is exactly the
    /// `editing` (gate armed) vs `working` (gate paid, still editing)
    /// distinction the session status renders.
    #[must_use]
    pub fn has_undelivered_any(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .any(|state| state.files.iter().any(|f| !f.delivered))
    }

    /// Returns `true` if the agent is currently in editing mode.
    #[must_use]
    pub fn is_editing(&self, session_id: Option<&str>, agent_id: &str) -> bool {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&key)
    }

    /// Returns `true` if the agent's batch holds any covered file (delivered or
    /// not).
    #[must_use]
    pub fn has_files(&self, session_id: Option<&str>, agent_id: &str) -> bool {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .is_some_and(|state| !state.files.is_empty())
    }

    /// Returns `true` while the debt gate is armed: any covered file in the
    /// agent's batch is still undelivered (misc 141).
    ///
    /// This — not [`has_files`](Self::has_files) — is the gate predicate. A batch
    /// whose files have all been delivered (`catenary diagnostics` handed a
    /// covering receipt to a client) is complete: the gate disarms even though
    /// the batch is retained for the next covered edit to extend or discard.
    #[must_use]
    pub fn has_undelivered(&self, session_id: Option<&str>, agent_id: &str) -> bool {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .is_some_and(|state| state.files.iter().any(|f| !f.delivered))
    }

    /// Returns a snapshot of the whole batch's file paths without mutating it.
    ///
    /// Includes delivered files: a bare `catenary diagnostics` re-diagnoses the
    /// whole batch (a later edit to C can change A's diagnostics in the same
    /// crate), so the prepare snapshot carries every batch file. Leaves the
    /// editing state intact.
    #[must_use]
    pub fn files(&self, session_id: Option<&str>, agent_id: &str) -> Vec<PathBuf> {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|state| state.files.iter().map(|f| f.path.clone()).collect())
            .unwrap_or_default()
    }

    /// Returns the batch's still-undelivered file paths — the outstanding debt.
    ///
    /// The boundary-block message lists these (the files that "haven't been
    /// diagnosed yet"), leaving delivered files out. Leaves the editing state
    /// intact.
    #[must_use]
    pub fn undelivered_files(&self, session_id: Option<&str>, agent_id: &str) -> Vec<PathBuf> {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|state| {
                state
                    .files
                    .iter()
                    .filter(|f| !f.delivered)
                    .map(|f| f.path.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Returns a snapshot of the agent's skipped-edits buckets (misc 173)
    /// without draining the editing state.
    ///
    /// Companion to [`files`](Self::files): together they let the
    /// `pre-tool/editing-stop` prepare hook *snapshot* the accumulated set —
    /// batch files plus skip buckets — into the handoff without clearing the
    /// accumulator. The clear is deferred to delivery (drain-on-consume,
    /// bug 32), so a failed `catenary diagnostics` attempt that never consumes
    /// leaves the set intact for a retry.
    #[must_use]
    pub fn skipped(&self, session_id: Option<&str>, agent_id: &str) -> SkippedEdits {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|state| state.skipped.clone())
            .unwrap_or_default()
    }

    /// Records a covered edit into the agent's batch (misc 141).
    ///
    /// The join / flip / discard rule, applied identically wherever a covered
    /// write is recorded (Edit/Write and resolved shell writes):
    ///
    /// - **Batch incomplete** (some flag false, or the batch is empty): the file
    ///   joins the batch if new, or its flag flips back to false if already
    ///   present. The iteration extends.
    /// - **Batch complete** (non-empty, all flags true): the batch is discarded
    ///   and a new one starts with just this file. The flat discard rule — no
    ///   inside/outside distinction. The skipped-edits note metadata rides
    ///   through the discard (a skipped edit is not part of the covered batch),
    ///   so an unreported skipped edit is not silently dropped.
    ///
    /// A no-op if the agent is not in editing mode — the accumulation path only
    /// calls this for an agent already known to be editing (the entry exists).
    ///
    /// `existed_at_record` reports whether the target already existed on disk
    /// when this edit was recorded (bug 76): `true` for a write/edit to a
    /// pre-existing file, `false` for a to-be-created target. It seeds the
    /// [`BatchFile::materialized`] latch so [`reconcile`](Self::reconcile) can
    /// later tell a never-materialized phantom (a resolved write whose command
    /// failed) from a written-then-deleted real edit. Re-recording an existing
    /// entry only ever latches `materialized` true — a target once observed on
    /// disk is never demoted to a phantom by a later record that missed it.
    pub fn record_covered_edit(
        &self,
        session_id: Option<&str>,
        agent_id: &str,
        path: PathBuf,
        existed_at_record: bool,
    ) {
        let key = editing_key(session_id, agent_id);
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
        {
            if entry.is_complete() {
                // Post-completion covered edit: discard the batch, start a new one
                // with this file (unreported skipped-note metadata is preserved).
                entry.files.clear();
                entry.files.push(BatchFile {
                    path,
                    delivered: false,
                    materialized: existed_at_record,
                });
            } else if let Some(existing) = entry.files.iter_mut().find(|f| f.path == path) {
                // Already in the incomplete batch — a fresh edit re-arms its gate.
                existing.delivered = false;
                existing.materialized = existing.materialized || existed_at_record;
            } else {
                entry.files.push(BatchFile {
                    path,
                    delivered: false,
                    materialized: existed_at_record,
                });
            }
        }
    }

    /// Reconciles the agent's batch against disk, dropping never-materialized
    /// phantom entries and latching real new-file creations (bug 76).
    ///
    /// Runs at the two boundaries where a stale batch would do harm: the next
    /// write command's boundary block (phantom gate debt) and the diagnose
    /// prepare snapshot (phantom `[path does not exist]` receipt lines). For
    /// each not-yet-materialized entry, `exists` probes the target on disk: if
    /// present now it is a successful creation — latch `materialized` true; if
    /// still absent it is a phantom (a resolved write whose command never wrote
    /// it) and is dropped. Already-materialized entries are untouched, so a real
    /// edit that was written and later deleted survives to render its honest
    /// `[path does not exist]` receipt.
    ///
    /// `exists` is injected (rather than calling `Path::exists` inline) so tests
    /// can drive the phantom / real / deleted cases deterministically without
    /// touching the filesystem. A no-op if the agent has no batch.
    pub fn reconcile(
        &self,
        session_id: Option<&str>,
        agent_id: &str,
        exists: impl Fn(&Path) -> bool,
    ) {
        let key = editing_key(session_id, agent_id);
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
        {
            entry.files.retain_mut(|file| {
                if file.materialized {
                    // Once observed on disk, an entry is real forever — a later
                    // deletion is reported honestly, not pruned as a phantom.
                    return true;
                }
                if exists(&file.path) {
                    // A to-be-created target that came to exist: a successful
                    // new-file write. Latch it and keep it in the batch.
                    file.materialized = true;
                    true
                } else {
                    // Never materialized and still absent — a phantom recorded
                    // by a command that never wrote it. Drop it: it arms no gate
                    // and prints no receipt line.
                    false
                }
            });
        }
    }

    /// Flips every covered file in the agent's batch to delivered (misc 141).
    ///
    /// The bare-`catenary diagnostics` payment: it diagnoses the whole batch, so
    /// on a successful response delivery every file's flag flips true. Called
    /// only *after* the socket write succeeds — a failed write leaves the flags
    /// false and the gate armed. A no-op if the agent has no batch.
    ///
    /// A bare delivery also hands the skipped-edits note to the client (the
    /// receipt renders both buckets), so the report debt is paid: the buckets
    /// reset here, otherwise one skipped edit would ride every later receipt
    /// for the rest of the session (misc 173). Scoped delivery
    /// ([`mark_delivered`](Self::mark_delivered)) suppresses the note and
    /// leaves the buckets intact — still unreported, they survive to the next
    /// bare run.
    pub fn mark_delivered_all(&self, session_id: Option<&str>, agent_id: &str) {
        let key = editing_key(session_id, agent_id);
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
        {
            for file in &mut entry.files {
                file.delivered = true;
            }
            entry.skipped = SkippedEdits::default();
        }
    }

    /// Flips exactly the named covered files to delivered (misc 141).
    ///
    /// The scoped-`catenary diagnostics path…` payment: partial delivery is the
    /// flag-flip mechanism itself, so the gate holds for any batch file left
    /// unnamed. A named path not in the batch is a no-op (a scoped call on a
    /// never-edited file has no flag to flip). Called only *after* the socket
    /// write succeeds. Matching is by exact path equality; the caller resolves
    /// relative paths to absolute before dispatch.
    pub fn mark_delivered(&self, session_id: Option<&str>, agent_id: &str, files: &[PathBuf]) {
        let key = editing_key(session_id, agent_id);
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
        {
            for file in &mut entry.files {
                if files.contains(&file.path) {
                    file.delivered = true;
                }
            }
        }
    }

    /// Exits editing mode for an agent, removing the entry entirely.
    pub fn done_editing(&self, session_id: Option<&str>, agent_id: &str) {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.remove(&key);
    }

    /// Records that a shell write was skipped during accumulation because it
    /// landed outside every tracked workspace root.
    ///
    /// Counted per `(session_id, agent_id)` so the drain reports the
    /// requesting agent's own skipped count (bug 37). A no-op if the agent
    /// is not in editing mode — the accumulation path only calls this for an
    /// agent already known to be editing.
    pub fn increment_outside(&self, session_id: Option<&str>, agent_id: &str) {
        let key = editing_key(session_id, agent_id);
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
        {
            entry.skipped.outside += 1;
        }
    }

    /// Records an edit made outside every tracked workspace root,
    /// **creating** the editing entry if none exists yet.
    ///
    /// Unlike [`increment_outside`](Self::increment_outside) — a no-op until an
    /// editing entry exists — this ensures a *standalone* out-of-root edit (one
    /// with no covering edit alongside it to open the entry) is still counted, so
    /// a later bare `catenary diagnostics` surfaces it instead of the bare
    /// `[no edited files]` lie (ephemeral-roots ticket 01 / bug 58). The entry it
    /// creates carries no files, so it never trips the boundary block and is
    /// silently cleared at the agent's stop if never diagnosed.
    ///
    /// `root` is the edit's enclosing project root when detectable
    /// (walk `.git` up from the path), recorded for the root-aware note; `None`
    /// only bumps the count.
    pub fn record_outside_edit(
        &self,
        session_id: Option<&str>,
        agent_id: &str,
        root: Option<PathBuf>,
    ) {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state.entry(key).or_default();
        entry.skipped.outside += 1;
        if let Some(root) = root {
            entry.skipped.outside_roots.insert(root);
        }
        drop(state);
    }

    /// Records an in-root edit no diagnostic feeder covers (misc 173),
    /// **creating** the editing entry if none exists yet.
    ///
    /// The sibling of [`record_outside_edit`](Self::record_outside_edit) for
    /// the other skip predicate: the file IS under a tracked root, but no
    /// server or linter covers it (`Makefile`, `.txt`, logs). Recording the
    /// file's display `name` lets the bare-run note say what went unchecked
    /// instead of misattributing the skip to root containment. Standalone
    /// semantics match `record_outside_edit`: the entry it creates carries no
    /// files, never trips the boundary block, and is silently cleared at the
    /// agent's stop if never diagnosed.
    pub fn record_uncovered_edit(&self, session_id: Option<&str>, agent_id: &str, name: String) {
        let key = editing_key(session_id, agent_id);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = state.entry(key).or_default();
        entry.skipped.uncovered += 1;
        entry.skipped.uncovered_files.insert(name);
        drop(state);
    }

    /// Clears all editing state. Returns the number of entries removed.
    ///
    /// Used by `SessionStart` cleanup to clear stale state when the
    /// agent's context is reset.
    pub fn clear_all(&self) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = state.len();
        state.clear();
        count
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn start_editing_enters_mode() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("should succeed");
        assert!(em.is_editing(None, "agent-a"));
    }

    #[test]
    fn start_editing_already_editing_errors() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("first call");
        let err = em
            .start_editing(None, "agent-a")
            .expect_err("should error on duplicate");
        assert!(
            err.to_string().contains("already in editing mode"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn is_editing_false_when_not_started() {
        let em = EditingManager::new();
        assert!(!em.is_editing(None, "agent-a"));
    }

    #[test]
    fn record_covered_edit_accumulates() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);
        em.record_covered_edit(None, "", PathBuf::from("/src/lib.rs"), true);
        let files = em.files(None, "");
        assert_eq!(files.len(), 2);
        // Fresh edits are undelivered — the gate is armed.
        assert!(em.has_undelivered(None, ""), "fresh edits arm the gate");
    }

    #[test]
    fn record_covered_edit_deduplicates() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);
        let files = em.files(None, "");
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn record_covered_edit_ignored_when_not_editing() {
        let em = EditingManager::new();
        em.record_covered_edit(None, "ghost", PathBuf::from("/src/main.rs"), true);
        assert!(em.files(None, "ghost").is_empty());
    }

    #[test]
    fn daemon_restart_releases_the_debt() {
        // Leg 4 (misc 160 / bug 79 maintainer ruling): the batch is in-memory
        // daemon state, released with the instance. On daemon death the debt is
        // dropped — not spooled — so a fresh daemon disarms the gate and a bare
        // run answers `[no edited files]`. Without this, an unstable daemon would
        // lock a session out of the shell.
        let sid = Some("session-1");

        // First daemon instance: an armed gate (undelivered covered edit).
        let before = EditingManager::new();
        before.start_editing(sid, "agent-a").expect("start");
        before.record_covered_edit(sid, "agent-a", PathBuf::from("/src/main.rs"), true);
        assert!(
            before.has_undelivered(sid, "agent-a"),
            "the gate must be armed before the restart"
        );

        // Daemon death + restart: the EditingManager is in-memory, owned by the
        // Session which the daemon process owns, so a fresh daemon starts with a
        // fresh, empty manager — the batch does not survive.
        drop(before);
        let after = EditingManager::new();

        // The gate is disarmed and the honest answer is `[no edited files]`
        // (no files, nothing undelivered) for the SAME identity.
        assert!(
            !after.has_undelivered(sid, "agent-a"),
            "daemon restart must disarm the gate — the debt is released, not spooled"
        );
        assert!(
            !after.has_files(sid, "agent-a"),
            "the restarted daemon has no batch for the identity — `[no edited files]`"
        );
        assert!(
            after.files(sid, "agent-a").is_empty(),
            "no batch files survive the restart"
        );
    }

    #[test]
    fn has_files_tracks_accumulation() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        assert!(!em.has_files(None, ""), "no files yet");
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);
        assert!(em.has_files(None, ""), "file added");
    }

    #[test]
    fn has_files_false_when_not_editing() {
        let em = EditingManager::new();
        assert!(!em.has_files(None, "ghost"));
    }

    #[test]
    fn files_snapshots_the_whole_batch() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);
        em.record_covered_edit(None, "", PathBuf::from("/src/lib.rs"), true);

        let snapshot = em.files(None, "");
        assert_eq!(
            snapshot,
            vec![PathBuf::from("/src/main.rs"), PathBuf::from("/src/lib.rs")],
        );
        // Snapshotting the batch does not mutate it — it stays fully tracked.
        assert!(em.has_files(None, ""), "files() must not mutate the batch");
        assert_eq!(em.files(None, "").len(), 2);
    }

    #[test]
    fn files_empty_when_not_editing() {
        let em = EditingManager::new();
        assert!(em.files(None, "ghost").is_empty());
    }

    #[test]
    fn skipped_reads_buckets_without_mutating() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"), true);
        em.increment_outside(None, "");
        em.increment_outside(None, "");

        assert_eq!(em.skipped(None, "").outside, 2);
        // Reading the skipped buckets must not touch the batch.
        assert!(
            em.has_files(None, ""),
            "skipped() must not mutate the batch"
        );
        // It remains readable on a repeat call (no consume).
        assert_eq!(em.skipped(None, "").outside, 2);
    }

    #[test]
    fn skipped_empty_when_not_editing() {
        let em = EditingManager::new();
        assert!(em.skipped(None, "ghost").is_empty());
    }

    #[test]
    fn record_outside_edit_creates_entry_when_standalone() {
        // The bug-58 case: an out-of-root edit arrives with no covered edit
        // alongside it, so no editing entry exists yet. `increment_outside`
        // would be a no-op; `record_outside_edit` creates the entry so the
        // count survives to the next bare `catenary diagnostics`.
        let em = EditingManager::new();
        assert!(!em.is_editing(None, ""), "no entry before the outside edit");
        em.record_outside_edit(None, "", Some(PathBuf::from("/home/dev/Lattice")));
        assert_eq!(
            em.skipped(None, "").outside,
            1,
            "count survives with no prior entry"
        );
        // The skipped-only entry carries no files, so it never trips the gate.
        assert!(
            !em.has_files(None, ""),
            "a skipped-only entry has no covered files"
        );
        assert_eq!(
            em.skipped(None, "").outside_roots,
            BTreeSet::from([PathBuf::from("/home/dev/Lattice")]),
            "the enclosing root rides along for root-aware wording"
        );
    }

    #[test]
    fn record_outside_edit_without_root_only_counts() {
        // No detectable enclosing root → the count bumps but no root is named.
        let em = EditingManager::new();
        em.record_outside_edit(None, "", None);
        em.record_outside_edit(None, "", None);
        assert_eq!(em.skipped(None, "").outside, 2);
        assert!(
            em.skipped(None, "").outside_roots.is_empty(),
            "no root recorded when none was detectable"
        );
    }

    #[test]
    fn record_outside_edit_dedups_roots() {
        // Several outside edits under one project name the root once.
        let em = EditingManager::new();
        let root = PathBuf::from("/home/dev/Lattice");
        em.record_outside_edit(None, "", Some(root.clone()));
        em.record_outside_edit(None, "", Some(root.clone()));
        assert_eq!(em.skipped(None, "").outside, 2, "every outside edit counts");
        assert_eq!(
            em.skipped(None, "").outside_roots,
            BTreeSet::from([root]),
            "the distinct root is named once"
        );
    }

    #[test]
    fn record_uncovered_edit_creates_entry_and_names_files() {
        // Misc 173: an in-root edit no feeder covers lands in its OWN bucket —
        // distinct from outside-roots — carrying the file's display name so
        // the note can say what went unchecked. Standalone semantics match
        // `record_outside_edit`: the entry is created, holds no files, and
        // never trips the gate.
        let em = EditingManager::new();
        assert!(!em.is_editing(None, ""), "no entry before the edit");
        em.record_uncovered_edit(None, "", "Makefile".to_string());
        em.record_uncovered_edit(None, "", "Makefile".to_string());
        em.record_uncovered_edit(None, "", "notes.txt".to_string());

        let skipped = em.skipped(None, "");
        assert_eq!(skipped.uncovered, 3, "every uncovered edit counts");
        assert_eq!(
            skipped.uncovered_files,
            BTreeSet::from(["Makefile".to_string(), "notes.txt".to_string()]),
            "distinct display names, deduplicated"
        );
        assert_eq!(
            skipped.outside, 0,
            "in-root uncovered edits never land in the outside bucket"
        );
        assert!(
            !em.has_files(None, ""),
            "an uncovered-only entry has no covered files — it never gates"
        );
    }

    #[test]
    fn bare_delivery_clears_skipped_buckets() {
        // Misc 173 sibling: the bare receipt renders the skipped-edits note,
        // paying the report debt — so delivery must reset the buckets. Before
        // the fix nothing cleared them, and one outside edit rode every later
        // receipt for the rest of the session (8+ paid batches observed).
        let em = EditingManager::new();
        em.record_outside_edit(None, "", Some(PathBuf::from("/home/dev/other")));
        em.record_uncovered_edit(None, "", "Makefile".to_string());
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"), true);

        // The bare run delivers the receipt (note included) — buckets reset.
        em.mark_delivered_all(None, "");
        assert!(
            em.skipped(None, "").is_empty(),
            "bare delivery pays the note debt — the buckets must clear"
        );

        // A fresh covered-only batch afterwards carries NO stale trailer.
        em.record_covered_edit(None, "", PathBuf::from("/src/b.rs"), true);
        em.mark_delivered_all(None, "");
        assert!(
            em.skipped(None, "").is_empty(),
            "a later clean batch must not resurrect the skipped note"
        );
    }

    #[test]
    fn scoped_delivery_keeps_skipped_buckets() {
        // A scoped pull suppresses the note (it names files explicitly), so
        // the skip record is still unreported: it must survive to the next
        // bare run rather than vanish silently.
        let em = EditingManager::new();
        em.record_outside_edit(None, "", None);
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"), true);

        em.mark_delivered(None, "", &[PathBuf::from("/src/a.rs")]);
        assert_eq!(
            em.skipped(None, "").outside,
            1,
            "scoped delivery renders no note — the bucket stays for the next bare run"
        );
    }

    // ── Batch delivery / gate (misc 141) ──────────────────────────────

    #[test]
    fn mark_delivered_all_disarms_the_gate_but_retains_the_batch() {
        // A bare `catenary diagnostics` diagnoses the whole batch and flips every
        // flag on successful delivery. The gate disarms, but the batch is
        // retained so a repeat bare run re-diagnoses the same scope.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"), true);
        em.record_covered_edit(None, "", PathBuf::from("/src/b.rs"), true);
        assert!(em.has_undelivered(None, ""), "fresh batch is undelivered");

        em.mark_delivered_all(None, "");
        assert!(
            !em.has_undelivered(None, ""),
            "delivery flips every flag — the gate disarms"
        );
        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/src/a.rs"), PathBuf::from("/src/b.rs")],
            "the batch is retained for a repeat bare run to re-diagnose"
        );
        assert!(
            em.undelivered_files(None, "").is_empty(),
            "no outstanding debt after full delivery"
        );
    }

    #[test]
    fn mid_batch_edit_flips_without_reset() {
        // A covered edit while the batch is incomplete (some flag false) joins /
        // re-arms without discarding the batch.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"), true);
        em.record_covered_edit(None, "", PathBuf::from("/src/b.rs"), true);
        // Deliver only a.rs (scoped), leaving b.rs undelivered → incomplete.
        em.mark_delivered(None, "", &[PathBuf::from("/src/a.rs")]);
        assert_eq!(
            em.undelivered_files(None, ""),
            vec![PathBuf::from("/src/b.rs")],
            "scoped delivery flips only the named file"
        );

        // A fresh edit to the already-delivered a.rs re-arms it — no reset.
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"), true);
        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/src/a.rs"), PathBuf::from("/src/b.rs")],
            "the batch is unchanged in membership — a.rs was not re-appended"
        );
        assert_eq!(
            em.undelivered_files(None, ""),
            vec![PathBuf::from("/src/a.rs"), PathBuf::from("/src/b.rs")],
            "editing a.rs again flips it back to undelivered"
        );
    }

    #[test]
    fn post_completion_edit_discards_and_starts_fresh_batch() {
        // A covered edit while the batch is complete (all flags true) discards
        // the batch and starts a new one with just that file.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"), true);
        em.record_covered_edit(None, "", PathBuf::from("/src/b.rs"), true);
        em.mark_delivered_all(None, "");

        // Now the batch is complete → the next covered edit resets it.
        em.record_covered_edit(None, "", PathBuf::from("/src/c.rs"), true);
        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/src/c.rs")],
            "a post-completion covered edit discards the old batch"
        );
        assert!(
            em.has_undelivered(None, ""),
            "the new batch's sole file is undelivered — the gate re-arms"
        );
    }

    #[test]
    fn empty_batch_is_not_complete_so_covered_edit_joins() {
        // A skipped-only entry (no covered files) is not a *complete* batch, so
        // the first covered edit joins it rather than discarding it — the
        // skipped metadata survives to the next bare run.
        let em = EditingManager::new();
        em.record_outside_edit(None, "", Some(PathBuf::from("/home/dev/Lattice")));
        assert!(!em.has_files(None, ""), "no covered files yet");

        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"), true);
        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/src/a.rs")],
            "the covered edit joins the skipped-only entry"
        );
        assert_eq!(
            em.skipped(None, "").outside,
            1,
            "the outside count survives the join"
        );
    }

    #[test]
    fn mark_delivered_ignores_unnamed_and_missing() {
        // A scoped delivery flips only the named files; a name not in the batch
        // is a no-op.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"), true);
        em.record_covered_edit(None, "", PathBuf::from("/src/b.rs"), true);

        em.mark_delivered(
            None,
            "",
            &[PathBuf::from("/src/a.rs"), PathBuf::from("/src/never.rs")],
        );
        assert_eq!(
            em.undelivered_files(None, ""),
            vec![PathBuf::from("/src/b.rs")],
            "only a.rs flips; the unknown name is ignored and b.rs stays armed"
        );
    }

    #[test]
    fn done_editing_removes_entry() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start");
        em.done_editing(None, "agent-a");
        assert!(!em.is_editing(None, "agent-a"));
        // Can re-enter after done
        em.start_editing(None, "agent-a").expect("re-enter");
        assert!(em.is_editing(None, "agent-a"));
    }

    #[test]
    fn scoped_delivery_pays_partial_debt() {
        // A scoped `catenary diagnostics a.rs` flips only a.rs; the gate holds
        // for b.rs, and the batch keeps both files.
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start");
        em.record_covered_edit(None, "agent-a", PathBuf::from("/src/a.rs"), true);
        em.record_covered_edit(None, "agent-a", PathBuf::from("/src/b.rs"), true);

        em.mark_delivered(None, "agent-a", &[PathBuf::from("/src/a.rs")]);
        assert_eq!(
            em.undelivered_files(None, "agent-a"),
            vec![PathBuf::from("/src/b.rs")],
            "the unnamed file stays armed after a scoped delivery"
        );
        assert!(
            em.has_undelivered(None, "agent-a"),
            "partial pay leaves the gate armed"
        );
        assert_eq!(
            em.files(None, "agent-a").len(),
            2,
            "the batch retains both files"
        );
    }

    #[test]
    fn is_active_tracks_any_accumulator() {
        let em = EditingManager::new();
        assert!(!em.is_active(), "no accumulator yet");
        em.start_editing(Some("s1"), "agent").expect("start");
        assert!(em.is_active(), "active after start, even with no files");
        em.done_editing(Some("s1"), "agent");
        assert!(!em.is_active(), "inactive after done_editing");

        // done_editing on the only entry also clears activity.
        em.start_editing(None, "a").expect("start");
        assert!(em.is_active());
        em.done_editing(None, "a");
        assert!(!em.is_active());
    }

    #[test]
    fn clear_all_empties_state() {
        let em = EditingManager::new();
        em.start_editing(None, "agent-a").expect("start a");
        em.start_editing(None, "agent-b").expect("start b");
        em.record_covered_edit(None, "agent-a", PathBuf::from("/src/main.rs"), true);
        let count = em.clear_all();
        assert_eq!(count, 2);
        assert!(!em.is_editing(None, "agent-a"));
        assert!(!em.is_editing(None, "agent-b"));
    }

    // ── Session-scoped key tests ───────────────────────────────────────

    #[test]
    fn different_sessions_same_agent_id_are_independent() {
        let em = EditingManager::new();
        em.start_editing(Some("session-a"), "")
            .expect("session A start");
        em.start_editing(Some("session-b"), "")
            .expect("session B start");

        assert!(em.is_editing(Some("session-a"), ""));
        assert!(em.is_editing(Some("session-b"), ""));

        em.done_editing(Some("session-a"), "");
        assert!(!em.is_editing(Some("session-a"), ""));
        assert!(em.is_editing(Some("session-b"), ""));
    }

    #[test]
    fn session_scoped_file_accumulation() {
        let em = EditingManager::new();
        em.start_editing(Some("s1"), "").expect("start");
        em.start_editing(Some("s2"), "").expect("start");

        em.record_covered_edit(Some("s1"), "", PathBuf::from("/a.rs"), true);
        em.record_covered_edit(Some("s2"), "", PathBuf::from("/b.rs"), true);

        assert_eq!(em.files(Some("s1"), ""), vec![PathBuf::from("/a.rs")]);
        assert_eq!(em.files(Some("s2"), ""), vec![PathBuf::from("/b.rs")]);
    }

    /// Two agents within ONE session (a subagent and the main agent, which
    /// share `session_id` and differ only by `agent_id`) accumulate into
    /// separate batches. Delivering one agent's batch must leave the other's
    /// files — and gate — intact. Regression guard for bug 37, where the
    /// diagnostics drain flattened every agent's bucket. Two `(session, agent)`
    /// pairs never share a batch (misc 141).
    #[test]
    fn agent_scoped_delivery_within_one_session() {
        let em = EditingManager::new();
        em.start_editing(Some("S"), "subA").expect("subA start");
        em.start_editing(Some("S"), "").expect("main start");

        // Distinct covered files per agent.
        em.record_covered_edit(Some("S"), "subA", PathBuf::from("/a.rs"), true);
        em.record_covered_edit(Some("S"), "", PathBuf::from("/b.rs"), true);
        // Distinct skipped-no-coverage counts per agent.
        em.increment_outside(Some("S"), "subA");
        em.increment_outside(Some("S"), "subA");
        em.increment_outside(Some("S"), "");
        assert_eq!(
            em.skipped(Some("S"), "subA").outside,
            2,
            "outside count attributed to subA, not shared"
        );

        // Deliver only the subagent's batch (its bare diagnostics run).
        em.mark_delivered_all(Some("S"), "subA");
        assert!(
            !em.has_undelivered(Some("S"), "subA"),
            "subA's gate disarms after its delivery"
        );
        assert!(
            em.skipped(Some("S"), "subA").is_empty(),
            "subA's bare delivery pays its own note debt (misc 173)"
        );

        // The main agent's batch is untouched — still armed.
        assert!(
            em.has_undelivered(Some("S"), ""),
            "the main agent's gate stays armed after subA's delivery"
        );
        assert_eq!(
            em.files(Some("S"), ""),
            vec![PathBuf::from("/b.rs")],
            "main agent's batch survives subA's delivery (bug 37)"
        );
        assert_eq!(
            em.skipped(Some("S"), "").outside,
            1,
            "main agent keeps its own skipped count"
        );
    }

    #[test]
    fn none_session_does_not_collide_with_some_session() {
        let em = EditingManager::new();
        em.start_editing(None, "agent").expect("no session");
        em.start_editing(Some("sess"), "agent")
            .expect("with session");

        assert!(em.is_editing(None, "agent"));
        assert!(em.is_editing(Some("sess"), "agent"));

        em.done_editing(None, "agent");
        assert!(!em.is_editing(None, "agent"));
        assert!(em.is_editing(Some("sess"), "agent"));
    }

    #[test]
    fn editing_key_format() {
        assert_eq!(editing_key(None, "agent"), "agent");
        assert_eq!(editing_key(Some(""), "agent"), "agent");
        assert_eq!(editing_key(Some("sess-1"), ""), "sess-1\0");
        assert_eq!(editing_key(Some("sess-1"), "sub"), "sess-1\0sub");
    }

    // ── Phantom reconciliation (bug 76) ────────────────────────────────

    #[test]
    fn reconcile_drops_never_materialized_phantom() {
        // The sighting: a write set resolved and recorded at PreToolUse, whose
        // command then failed wholesale (zero bytes written). The target never
        // came to exist — recorded not-materialized, still absent at reconcile.
        // It must be dropped so it arms no gate and prints no receipt line.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/phantom.rs"), false);
        assert!(em.has_undelivered(None, ""), "recorded → gate armed");

        // The command failed: the target still does not exist on disk.
        em.reconcile(None, "", |_| false);

        assert!(
            em.files(None, "").is_empty(),
            "the phantom entry is dropped from the batch"
        );
        assert!(
            !em.has_undelivered(None, ""),
            "no phantom gate debt survives — future work is not gated on it"
        );
    }

    #[test]
    fn reconcile_keeps_real_edit_that_still_exists() {
        // A write/edit to a pre-existing file: recorded materialized. Reconcile
        // must keep it — a real edit still gates until diagnosed.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/real.rs"), true);

        // Even a probe that would report absence must not drop a materialized
        // entry (it was observed on disk at record time).
        em.reconcile(None, "", |_| false);

        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/real.rs")],
            "a real edit survives reconciliation"
        );
        assert!(em.has_undelivered(None, ""), "the real edit still gates");
    }

    #[test]
    fn reconcile_latches_successful_new_file_creation() {
        // A to-be-created target recorded not-materialized whose command DID
        // write it: reconcile observes it on disk and latches it materialized,
        // keeping it in the batch to gate and be reported like any real edit.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/created.rs"), false);

        // First pass: the file now exists (the write succeeded) → latch + keep.
        em.reconcile(None, "", |_| true);
        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/created.rs")],
            "a successful new-file creation is kept"
        );
        assert!(em.has_undelivered(None, ""), "the created file gates");

        // Latched: a later pass reporting absence (the file was deleted) must
        // NOT drop it — a written-then-deleted real edit is reported honestly,
        // not pruned as a phantom.
        em.reconcile(None, "", |_| false);
        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/created.rs")],
            "once materialized, a later deletion does not prune the entry"
        );
    }

    #[test]
    fn reconcile_keeps_written_then_deleted_real_edit() {
        // A pre-existing file edited (materialized) and then deleted: it is a
        // real edit that vanished. Reconcile must keep it so `catenary
        // diagnostics` reports its nonexistence honestly — NOT hide it. This is
        // the constraint that distinguishes bug 76 from over-pruning.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/deleted.rs"), true);

        // The file no longer exists on disk, yet it was genuinely edited.
        em.reconcile(None, "", |_| false);

        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/deleted.rs")],
            "a written-then-deleted real edit is not pruned"
        );
        assert!(
            em.has_undelivered(None, ""),
            "the vanished real edit still gates — its receipt is the honest report"
        );
    }

    #[test]
    fn reconcile_mixed_batch_drops_only_the_phantom() {
        // A batch mixing a real edit and a phantom (the failed-apply shape:
        // some targets landed, others never wrote): reconcile drops only the
        // never-materialized phantom, leaving the real edit's gate intact.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/kept.rs"), true);
        em.record_covered_edit(None, "", PathBuf::from("/phantom.rs"), false);

        // Only the real edit exists on disk.
        em.reconcile(None, "", |p| p == Path::new("/kept.rs"));

        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/kept.rs")],
            "only the phantom is dropped; the real edit survives"
        );
        assert!(
            em.has_undelivered(None, ""),
            "the surviving real edit keeps the gate armed"
        );
    }

    #[test]
    fn reconcile_re_record_latches_materialized() {
        // A target first recorded not-materialized, then re-recorded once it
        // exists (a second edit after the create landed), latches materialized —
        // so a later reconcile with an absence probe does not prune it.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/f.rs"), false);
        // Second edit, now that the file exists: re-record with existed=true.
        em.record_covered_edit(None, "", PathBuf::from("/f.rs"), true);
        assert_eq!(em.files(None, "").len(), 1, "re-record does not duplicate");

        em.reconcile(None, "", |_| false);
        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/f.rs")],
            "re-recording with existed=true latches materialized — not pruned"
        );
    }

    #[test]
    fn reconcile_is_noop_for_agent_without_batch() {
        // A defensive no-op: reconciling an agent with no editing entry must not
        // panic or create state.
        let em = EditingManager::new();
        em.reconcile(None, "ghost", |_| false);
        assert!(!em.is_editing(None, "ghost"), "reconcile creates no entry");
    }
}
