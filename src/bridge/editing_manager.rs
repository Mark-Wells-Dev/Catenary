// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! In-memory editing state manager.
//!
//! Tracks which agents are in editing mode and accumulates modified file
//! paths during editing sessions. State is per-session lifetime — no
//! database persistence needed since the session owns the [`super::session::Session`]
//! which owns the `EditingManager`.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Result, anyhow};

/// Computes the editing state key from session and agent identifiers.
///
/// The key is `"{session_id}\0{agent_id}"` when a non-empty session ID is
/// present, falling back to just `agent_id` for backward compatibility
/// with hosts that don't provide a session ID.
fn editing_key(session_id: Option<&str>, agent_id: &str) -> String {
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
/// flag is false. The batch is durable daemon state — diagnostics are always
/// recomputed over it — so repeat bare runs re-diagnose the same scope.
///
/// Holds the covered batch alongside the count of files skipped during
/// accumulation because they lacked LSP coverage (outside tracked roots), so a
/// per-agent read reports the requesting agent's own skipped-no-coverage count,
/// never another agent's (bug 37). Uncovered edits carry no flag: they never
/// join the batch and never discard it (bug 58 note behavior unchanged).
#[derive(Default)]
struct EditingState {
    /// The current batch: covered files, each with a `delivered` flag.
    files: Vec<BatchFile>,
    /// Files skipped during accumulation for lack of LSP coverage.
    filtered: usize,
    /// Distinct enclosing project roots of the filtered (out-of-root) edits,
    /// for the root-aware bare-run note (ephemeral-roots ticket 01 / bug 58).
    /// Empty when a filtered edit had no detectable enclosing project root.
    filtered_roots: BTreeSet<PathBuf>,
}

impl EditingState {
    /// Whether the batch is complete: non-empty and every file delivered.
    ///
    /// An empty batch (no covered files — e.g. a filtered-only entry) is *not*
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

    /// Returns the count of files skipped during accumulation for lack of LSP
    /// coverage, without draining the editing state.
    ///
    /// Companion to [`files`](Self::files): together they let the
    /// `pre-tool/editing-stop` prepare hook *snapshot* the accumulated set —
    /// file list plus filtered count — into the handoff without clearing the
    /// accumulator. The clear is deferred to the consume step (drain-on-consume,
    /// bug 32), so a failed `catenary diagnostics` attempt that never consumes
    /// leaves the set intact for a retry.
    #[must_use]
    pub fn filtered(&self, session_id: Option<&str>, agent_id: &str) -> usize {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map_or(0, |state| state.filtered)
    }

    /// Returns the distinct enclosing project roots of the agent's filtered
    /// (out-of-root) edits, without draining the editing state.
    ///
    /// Companion to [`filtered`](Self::filtered): the `pre-tool/editing-stop`
    /// prepare hook snapshots both into the handoff so a bare `catenary
    /// diagnostics` can name the roots that have no language servers running
    /// (ephemeral-roots ticket 01 / bug 58). Empty when no filtered edit
    /// carried a detectable enclosing root.
    #[must_use]
    pub fn filtered_roots(&self, session_id: Option<&str>, agent_id: &str) -> BTreeSet<PathBuf> {
        let key = editing_key(session_id, agent_id);
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .map(|state| state.filtered_roots.clone())
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
    ///   inside/outside distinction. The out-of-coverage note metadata rides
    ///   through the discard (a filtered edit is not part of the covered batch),
    ///   so an unreported out-of-root edit is not silently dropped.
    ///
    /// A no-op if the agent is not in editing mode — the accumulation path only
    /// calls this for an agent already known to be editing (the entry exists).
    pub fn record_covered_edit(&self, session_id: Option<&str>, agent_id: &str, path: PathBuf) {
        let key = editing_key(session_id, agent_id);
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
        {
            if entry.is_complete() {
                // Post-completion covered edit: discard the batch, start a new one
                // with this file (the filtered note metadata is preserved).
                entry.files.clear();
                entry.files.push(BatchFile {
                    path,
                    delivered: false,
                });
            } else if let Some(existing) = entry.files.iter_mut().find(|f| f.path == path) {
                // Already in the incomplete batch — a fresh edit re-arms its gate.
                existing.delivered = false;
            } else {
                entry.files.push(BatchFile {
                    path,
                    delivered: false,
                });
            }
        }
    }

    /// Flips every covered file in the agent's batch to delivered (misc 141).
    ///
    /// The bare-`catenary diagnostics` payment: it diagnoses the whole batch, so
    /// on a successful response delivery every file's flag flips true. Called
    /// only *after* the socket write succeeds — a failed write leaves the flags
    /// false and the gate armed. A no-op if the agent has no batch.
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

    /// Records that a file was skipped during accumulation because it
    /// lacked LSP coverage (outside tracked workspace roots).
    ///
    /// Counted per `(session_id, agent_id)` so the drain reports the
    /// requesting agent's own skipped count (bug 37). A no-op if the agent
    /// is not in editing mode — the accumulation path only calls this for an
    /// agent already known to be editing.
    pub fn increment_filtered(&self, session_id: Option<&str>, agent_id: &str) {
        let key = editing_key(session_id, agent_id);
        if let Some(entry) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&key)
        {
            entry.filtered += 1;
        }
    }

    /// Records a filtered (out-of-root / uncovered) edit for an agent,
    /// **creating** the editing entry if none exists yet.
    ///
    /// Unlike [`increment_filtered`](Self::increment_filtered) — a no-op until an
    /// editing entry exists — this ensures a *standalone* out-of-root edit (one
    /// with no covering edit alongside it to open the entry) is still counted, so
    /// a later bare `catenary diagnostics` surfaces it instead of the bare
    /// `[no edited files]` lie (ephemeral-roots ticket 01 / bug 58). The entry it
    /// creates carries no files, so it never trips the boundary block and is
    /// silently cleared at the agent's stop if never diagnosed.
    ///
    /// `root` is the filtered edit's enclosing project root when detectable
    /// (walk `.git` up from the path), recorded for the root-aware note; `None`
    /// only bumps the count.
    pub fn record_filtered_edit(
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
        entry.filtered += 1;
        if let Some(root) = root {
            entry.filtered_roots.insert(root);
        }
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
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"));
        em.record_covered_edit(None, "", PathBuf::from("/src/lib.rs"));
        let files = em.files(None, "");
        assert_eq!(files.len(), 2);
        // Fresh edits are undelivered — the gate is armed.
        assert!(em.has_undelivered(None, ""), "fresh edits arm the gate");
    }

    #[test]
    fn record_covered_edit_deduplicates() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"));
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"));
        let files = em.files(None, "");
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn record_covered_edit_ignored_when_not_editing() {
        let em = EditingManager::new();
        em.record_covered_edit(None, "ghost", PathBuf::from("/src/main.rs"));
        assert!(em.files(None, "ghost").is_empty());
    }

    #[test]
    fn has_files_tracks_accumulation() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        assert!(!em.has_files(None, ""), "no files yet");
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"));
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
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"));
        em.record_covered_edit(None, "", PathBuf::from("/src/lib.rs"));

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
    fn filtered_reads_count_without_mutating() {
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/main.rs"));
        em.increment_filtered(None, "");
        em.increment_filtered(None, "");

        assert_eq!(em.filtered(None, ""), 2);
        // Reading the filtered count must not touch the batch.
        assert!(
            em.has_files(None, ""),
            "filtered() must not mutate the batch"
        );
        // It remains readable on a repeat call (no consume).
        assert_eq!(em.filtered(None, ""), 2);
    }

    #[test]
    fn filtered_zero_when_not_editing() {
        let em = EditingManager::new();
        assert_eq!(em.filtered(None, "ghost"), 0);
    }

    #[test]
    fn record_filtered_edit_creates_entry_when_standalone() {
        // The bug-58 case: an out-of-root edit arrives with no covered edit
        // alongside it, so no editing entry exists yet. `increment_filtered`
        // would be a no-op; `record_filtered_edit` creates the entry so the
        // count survives to the next bare `catenary diagnostics`.
        let em = EditingManager::new();
        assert!(
            !em.is_editing(None, ""),
            "no entry before the filtered edit"
        );
        em.record_filtered_edit(None, "", Some(PathBuf::from("/home/dev/Lattice")));
        assert_eq!(
            em.filtered(None, ""),
            1,
            "count survives with no prior entry"
        );
        // The filtered-only entry carries no files, so it never trips the gate.
        assert!(
            !em.has_files(None, ""),
            "a filtered-only entry has no covered files"
        );
        assert_eq!(
            em.filtered_roots(None, ""),
            BTreeSet::from([PathBuf::from("/home/dev/Lattice")]),
            "the enclosing root rides along for root-aware wording"
        );
    }

    #[test]
    fn record_filtered_edit_without_root_only_counts() {
        // No detectable enclosing root → the count bumps but no root is named.
        let em = EditingManager::new();
        em.record_filtered_edit(None, "", None);
        em.record_filtered_edit(None, "", None);
        assert_eq!(em.filtered(None, ""), 2);
        assert!(
            em.filtered_roots(None, "").is_empty(),
            "no root recorded when none was detectable"
        );
    }

    #[test]
    fn record_filtered_edit_dedups_roots() {
        // Several filtered edits under one project name the root once.
        let em = EditingManager::new();
        let root = PathBuf::from("/home/dev/Lattice");
        em.record_filtered_edit(None, "", Some(root.clone()));
        em.record_filtered_edit(None, "", Some(root.clone()));
        assert_eq!(em.filtered(None, ""), 2, "every filtered edit counts");
        assert_eq!(
            em.filtered_roots(None, ""),
            BTreeSet::from([root]),
            "the distinct root is named once"
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
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"));
        em.record_covered_edit(None, "", PathBuf::from("/src/b.rs"));
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
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"));
        em.record_covered_edit(None, "", PathBuf::from("/src/b.rs"));
        // Deliver only a.rs (scoped), leaving b.rs undelivered → incomplete.
        em.mark_delivered(None, "", &[PathBuf::from("/src/a.rs")]);
        assert_eq!(
            em.undelivered_files(None, ""),
            vec![PathBuf::from("/src/b.rs")],
            "scoped delivery flips only the named file"
        );

        // A fresh edit to the already-delivered a.rs re-arms it — no reset.
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"));
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
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"));
        em.record_covered_edit(None, "", PathBuf::from("/src/b.rs"));
        em.mark_delivered_all(None, "");

        // Now the batch is complete → the next covered edit resets it.
        em.record_covered_edit(None, "", PathBuf::from("/src/c.rs"));
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
        // A filtered-only entry (no covered files) is not a *complete* batch, so
        // the first covered edit joins it rather than discarding it — the
        // filtered metadata survives to the next bare run.
        let em = EditingManager::new();
        em.record_filtered_edit(None, "", Some(PathBuf::from("/home/dev/Lattice")));
        assert!(!em.has_files(None, ""), "no covered files yet");

        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"));
        assert_eq!(
            em.files(None, ""),
            vec![PathBuf::from("/src/a.rs")],
            "the covered edit joins the filtered-only entry"
        );
        assert_eq!(
            em.filtered(None, ""),
            1,
            "the filtered count survives the join"
        );
    }

    #[test]
    fn mark_delivered_ignores_unnamed_and_missing() {
        // A scoped delivery flips only the named files; a name not in the batch
        // is a no-op.
        let em = EditingManager::new();
        em.start_editing(None, "").expect("start");
        em.record_covered_edit(None, "", PathBuf::from("/src/a.rs"));
        em.record_covered_edit(None, "", PathBuf::from("/src/b.rs"));

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
        em.record_covered_edit(None, "agent-a", PathBuf::from("/src/a.rs"));
        em.record_covered_edit(None, "agent-a", PathBuf::from("/src/b.rs"));

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
        em.record_covered_edit(None, "agent-a", PathBuf::from("/src/main.rs"));
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

        em.record_covered_edit(Some("s1"), "", PathBuf::from("/a.rs"));
        em.record_covered_edit(Some("s2"), "", PathBuf::from("/b.rs"));

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
        em.record_covered_edit(Some("S"), "subA", PathBuf::from("/a.rs"));
        em.record_covered_edit(Some("S"), "", PathBuf::from("/b.rs"));
        // Distinct skipped-no-coverage counts per agent.
        em.increment_filtered(Some("S"), "subA");
        em.increment_filtered(Some("S"), "subA");
        em.increment_filtered(Some("S"), "");

        // Deliver only the subagent's batch (its bare diagnostics run).
        em.mark_delivered_all(Some("S"), "subA");
        assert!(
            !em.has_undelivered(Some("S"), "subA"),
            "subA's gate disarms after its delivery"
        );
        assert_eq!(
            em.filtered(Some("S"), "subA"),
            2,
            "filtered attributed to subA, not shared"
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
            em.filtered(Some("S"), ""),
            1,
            "main agent keeps its own filtered count"
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
}
