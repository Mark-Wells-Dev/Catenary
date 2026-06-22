// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Bounded directory-deletion watch for subagent worktree roots.
//!
//! Workstream 30, ticket 05. The daemon brackets each `isolation:"worktree"`
//! subagent's git worktree to a `worktree:{session_id}:{path}` LSP root, mounted
//! at `SubagentStart` and meant to be torn down at `WorktreeRemove`
//! ([decision 021](../../../CatenaryInternal/decisions/021_worktree_root_bracketed_to_worktree_lifecycle.md)).
//! Live debugging found that **`WorktreeRemove` never fires for git worktrees**:
//! the host runs `git worktree remove` itself and invokes the hook only for the
//! non-git VCS / `--worktree` session-exit path. So the prompt teardown leg is
//! dead for the exact case it targets, and the root leaks until the hourly
//! dir-gone GC reclaims it (≤1 h).
//!
//! This module adds a *prompt* teardown signal: a bounded filesystem watch on
//! each mounted worktree dir that reaps the root the instant the dir is deleted.
//! It does **not** add new teardown logic — it is a new, fast *trigger* for the
//! existing reap (`remove_contributor` + `sync_roots`) the GC and the (dead)
//! `WorktreeRemove` handler already run. The hourly
//! [`crate::router::WORKTREE_ROOT_GC_INTERVAL`] GC stays as the crash-safe
//! backstop: this watch is in-memory and dies with the daemon.
//!
//! ## Why a watcher is allowed here (decision 018 carve-out)
//!
//! [Decision 018](../../../CatenaryInternal/decisions/018_filesystem_coherence_changed_set.md)
//! rejected owning an inotify watcher for filesystem *coherence* — recursive,
//! whole-tree watching of every project root. None of that rationale applies: this
//! is **bounded** (one non-recursive watch per live worktree dir — a dozen at most
//! in the dozens-of-subagents-per-batch workflow, never thousands) and watches a
//! single dir node for its own deletion, the genuine between-query-reactivity
//! resurrection trigger decision 018 explicitly named.
//!
//! ## Design
//!
//! - **Watch the parent, not the dir itself.** For a mounted worktree
//!   `…/worktrees/agent-abc`, we add a non-recursive watch on `…/worktrees/` and
//!   look for a delete event naming `agent-abc`. Watching the parent dodges the
//!   "watch auto-dropped when its own dir vanishes" subtlety and the
//!   register-then-delete race.
//! - **One watch per parent, refcounted.** Several worktrees often share a parent
//!   (`.claude/worktrees/`); the [`notify`] watch on that parent is added once and
//!   removed only when its last watched child is unregistered.
//! - **Coalesced reap.** `git worktree remove` deletes the children then the dir,
//!   so a single removal yields several delete events. Each drives the same reap
//!   in [`crate::router::SessionManager::spawn_worktree_watch_reaper`], which is
//!   idempotent — a double-reap from the watch, the GC, and the `SessionEnd` sweep
//!   is a harmless no-op.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use notify::Watcher;
use tracing::debug;

use crate::source::Source;

/// A reap event emitted by the watcher's [`notify`] callback: the deletion of a
/// watched worktree path. Carries the contributor key so the reaper task can run
/// the existing teardown without re-deriving it.
#[derive(Debug, Clone)]
pub struct WorktreeDeleted {
    /// The `worktree:{session_id}:{path}` contributor key whose dir was deleted.
    pub contributor: String,
}

/// Per-watch registration state, behind a single mutex shared with the
/// [`notify`] callback.
#[derive(Default)]
struct WatchState {
    /// `worktree:{session_id}:{path}` contributor key → the watched worktree
    /// path. Used to match a delete event's paths back to a contributor.
    contributors: HashMap<String, PathBuf>,
    /// Watched parent dir → number of live worktrees watched under it. The
    /// [`notify`] watch on a parent is added on the first child and removed when
    /// the last child unregisters (`git`'s default puts every subagent worktree
    /// under one `.claude/worktrees/`, so the parent watch is shared).
    parent_refs: HashMap<PathBuf, usize>,
}

/// Owns the [`notify`] watcher and the registration map for mounted worktree
/// dirs.
///
/// Cloneable (`Arc`-backed) so the daemon's hook-dispatch context, the reaper
/// task, and the GC all act on the same live state.
///
/// `None`-friendly at call sites: the daemon wires one in
/// [`crate::router::SessionManager::with_session`]; transport-only test managers
/// have none and the mount/teardown paths simply skip watch registration.
#[derive(Clone)]
pub struct WorktreeWatcher {
    /// The single OS watcher (inotify/FSEvents/kqueue). Behind a mutex because
    /// [`notify::Watcher::watch`]/`unwatch` take `&mut self` and we register from
    /// the hook-dispatch task while the callback fires on the watcher's thread.
    watcher: Arc<Mutex<notify::RecommendedWatcher>>,
    /// Shared registration state, also read by the callback to map a delete event
    /// to its contributor.
    state: Arc<Mutex<WatchState>>,
}

impl WorktreeWatcher {
    /// Creates the watcher and the channel the reaper task drains.
    ///
    /// The returned [`tokio::sync::mpsc::UnboundedReceiver`] yields one
    /// [`WorktreeDeleted`] per watched worktree whose dir is deleted; the daemon
    /// hands it to a reaper task (see
    /// [`crate::router::SessionManager::spawn_worktree_watch_reaper`]). An
    /// unbounded channel keeps the [`notify`] callback non-blocking (it must never
    /// stall the OS watcher thread); the event volume is a handful of deletes per
    /// batch.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS watcher cannot be created (e.g.
    /// `max_user_instances` exhausted). The caller treats this as non-fatal — the
    /// hourly GC still reclaims leaked roots.
    pub fn new() -> anyhow::Result<(Self, tokio::sync::mpsc::UnboundedReceiver<WorktreeDeleted>)> {
        let state: Arc<Mutex<WatchState>> = Arc::new(Mutex::new(WatchState::default()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let cb_state = Arc::clone(&state);
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else {
                return;
            };
            // Only deletion-shaped events reap. `git worktree remove` deletes the
            // children then the dir, so on Linux the named-child removal arrives as
            // `Remove`; matching on `is_remove_kind` keeps us robust across
            // backends (FSEvents coalesces; kqueue differs).
            if !is_remove_kind(event.kind) {
                return;
            }
            for path in &event.paths {
                if let Some(contributor) = matching_contributor(&cb_state, path) {
                    // Non-blocking; a closed channel (daemon shutting down) is fine.
                    let _ = tx.send(WorktreeDeleted { contributor });
                }
            }
        })?;

        Ok((
            Self {
                watcher: Arc::new(Mutex::new(watcher)),
                state,
            },
            rx,
        ))
    }

    /// Registers a watch for a mounted worktree.
    ///
    /// Watches the worktree's **parent** dir (non-recursive) for a delete naming
    /// the worktree, refcounted per parent so a shared `.claude/worktrees/` is
    /// watched once. Idempotent: re-registering the same contributor refreshes its
    /// path and does not double-count the parent.
    ///
    /// Best-effort — a failure to add the OS watch is logged at `debug` and
    /// swallowed; the hourly GC remains the backstop. Returns whether the OS watch
    /// is in place for this contributor.
    pub fn register(&self, contributor: &str, worktree: &Path) -> bool {
        let Some(parent) = worktree.parent().map(Path::to_path_buf) else {
            debug!(
                source = Source::DaemonDispatch.as_str(),
                contributor, "worktree watch skipped: path has no parent",
            );
            return false;
        };

        // Compute the registration change under `state`, but perform every
        // `notify` watch/unwatch AFTER dropping the lock — both block on the
        // watcher's callback thread, which itself locks `state` (see
        // `release_parent`).
        let (old_parent_to_unwatch, first) = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            // Re-registering the same contributor: refresh the path only. Release a
            // stale parent ref first if the path moved to a different parent (rare).
            let old_to_unwatch = match state
                .contributors
                .insert(contributor.to_string(), worktree.to_path_buf())
            {
                // Same parent — the existing parent ref already covers it.
                Some(old) if old.parent() == Some(parent.as_path()) => return true,
                Some(old) => Self::release_parent(&mut state, old.parent()),
                None => None,
            };
            let new_ref = state.parent_refs.entry(parent.clone()).or_insert(0);
            *new_ref += 1;
            let first = *new_ref == 1;
            drop(state);
            (old_to_unwatch, first)
        };

        // Drop a moved contributor's now-childless old parent watch, lock-free.
        if let Some(old_parent) = old_parent_to_unwatch {
            let _ = self
                .watcher
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .unwatch(&old_parent);
        }

        if first {
            // Add the OS watch on the parent only on its first child.
            let watch_result = self
                .watcher
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .watch(&parent, notify::RecursiveMode::NonRecursive);
            if let Err(e) = watch_result {
                // Roll back: drop the contributor entry and its just-added parent
                // ref so no phantom registration lingers (the OS watch never
                // attached). `parent_refs` removal makes a later unregister a clean
                // no-op.
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                state.contributors.remove(contributor);
                state.parent_refs.remove(&parent);
                drop(state);
                debug!(
                    source = Source::DaemonDispatch.as_str(),
                    contributor,
                    parent = %parent.display(),
                    "worktree deletion watch failed (GC remains the backstop): {e}",
                );
                return false;
            }
            debug!(
                source = Source::DaemonDispatch.as_str(),
                contributor,
                parent = %parent.display(),
                "registered worktree deletion watch",
            );
        }
        true
    }

    /// Unregisters a contributor's watch, dropping the parent's OS watch when its
    /// last watched child leaves.
    ///
    /// Idempotent: unknown contributors are a no-op, so the watch-reap, the
    /// `WorktreeRemove` handler, the `SessionEnd` sweep, and the GC can all call it
    /// for the same key without disagreeing.
    pub fn unregister(&self, contributor: &str) {
        let to_unwatch = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let removed = state.contributors.remove(contributor);
            let result =
                removed.and_then(|worktree| Self::release_parent(&mut state, worktree.parent()));
            drop(state);
            result
        };
        if let Some(parent) = to_unwatch {
            let _ = self
                .watcher
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .unwatch(&parent);
        }
    }

    /// Unregisters every contributor whose key `starts_with(prefix)` — the
    /// `worktree:{session_id}:` sweep the `SessionEnd` backstop runs.
    pub fn unregister_with_prefix(&self, prefix: &str) {
        let to_unwatch: Vec<PathBuf> = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            let keys: Vec<String> = state
                .contributors
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect();
            let mut parents = Vec::new();
            for key in keys {
                if let Some(worktree) = state.contributors.remove(&key)
                    && let Some(parent) = Self::release_parent(&mut state, worktree.parent())
                {
                    parents.push(parent);
                }
            }
            parents
        };
        for parent in to_unwatch {
            let _ = self
                .watcher
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .unwatch(&parent);
        }
    }

    /// Decrements a parent's refcount under the caller's `state` guard and returns
    /// the parent path to `unwatch` iff its last watched child just left. The
    /// caller MUST perform the `unwatch` **after** dropping the `state` lock.
    ///
    /// Why the split (deadlock avoidance): `notify`'s `unwatch` blocks until the
    /// watcher's event thread acknowledges, and that thread runs the deletion
    /// callback, which locks `state`. Calling `unwatch` while holding `state` thus
    /// deadlocks against a concurrent callback — e.g. a sibling worktree deleted
    /// under the same shared `…/worktrees/` parent during batch cleanup. Returning
    /// the path and unwatching lock-free removes the hazard entirely.
    fn release_parent(state: &mut WatchState, parent: Option<&Path>) -> Option<PathBuf> {
        let parent = parent?;
        let count = state.parent_refs.get_mut(parent)?;
        *count -= 1;
        if *count == 0 {
            state.parent_refs.remove(parent);
            Some(parent.to_path_buf())
        } else {
            None
        }
    }

    /// Returns whether a contributor currently has a watch registered (test/probe
    /// helper; the daemon paths key off the channel, not this).
    #[cfg(test)]
    #[must_use]
    pub fn is_registered(&self, contributor: &str) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contributors
            .contains_key(contributor)
    }

    /// Returns the number of distinct parent dirs currently watched (test helper).
    #[cfg(test)]
    #[must_use]
    pub fn watched_parent_count(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .parent_refs
            .len()
    }
}

/// Finds the contributor whose watched worktree path equals `deleted` (the parent
/// watch reports the full child path on a delete). Returns the key to reap, if
/// any.
fn matching_contributor(state: &Arc<Mutex<WatchState>>, deleted: &Path) -> Option<String> {
    let state = state.lock().unwrap_or_else(PoisonError::into_inner);
    state
        .contributors
        .iter()
        .find_map(|(key, path)| (path.as_path() == deleted).then(|| key.clone()))
}

/// Whether a [`notify`] event kind is a deletion. Kept liberal across backends:
/// inotify reports `Remove(RemoveKind::Folder)`, FSEvents/kqueue may report
/// `Remove(RemoveKind::Any)` or coalesce.
const fn is_remove_kind(kind: notify::EventKind) -> bool {
    matches!(kind, notify::EventKind::Remove(_))
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test code")]
mod tests {
    use super::{WorktreeDeleted, WorktreeWatcher, is_remove_kind, matching_contributor};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    #[test]
    fn is_remove_kind_matches_only_removals() {
        use notify::EventKind;
        use notify::event::{ModifyKind, RemoveKind};
        assert!(is_remove_kind(EventKind::Remove(RemoveKind::Folder)));
        assert!(is_remove_kind(EventKind::Remove(RemoveKind::Any)));
        assert!(!is_remove_kind(EventKind::Create(
            notify::event::CreateKind::Folder
        )));
        assert!(!is_remove_kind(EventKind::Modify(ModifyKind::Any)));
    }

    #[test]
    fn matching_contributor_keys_on_exact_path() {
        let state = Arc::new(Mutex::new(super::WatchState {
            contributors: HashMap::from([(
                "worktree:s1:/tmp/wt/agent-a".to_string(),
                PathBuf::from("/tmp/wt/agent-a"),
            )]),
            parent_refs: HashMap::new(),
        }));
        assert_eq!(
            matching_contributor(&state, &PathBuf::from("/tmp/wt/agent-a")),
            Some("worktree:s1:/tmp/wt/agent-a".to_string())
        );
        assert_eq!(
            matching_contributor(&state, &PathBuf::from("/tmp/wt/agent-b")),
            None
        );
        // The parent itself is not a contributor path.
        assert_eq!(
            matching_contributor(&state, &PathBuf::from("/tmp/wt")),
            None
        );
    }

    #[test]
    fn register_unregister_refcounts_shared_parent() {
        let (watcher, _rx) = WorktreeWatcher::new().expect("create watcher");
        let parent = tempfile::tempdir().expect("tempdir");
        let a = parent.path().join("agent-a");
        let b = parent.path().join("agent-b");
        std::fs::create_dir_all(&a).expect("mkdir a");
        std::fs::create_dir_all(&b).expect("mkdir b");

        let ka = format!("worktree:s1:{}", a.display());
        let kb = format!("worktree:s1:{}", b.display());

        assert!(watcher.register(&ka, &a));
        assert!(watcher.register(&kb, &b));
        // Shared parent — one OS watch.
        assert_eq!(watcher.watched_parent_count(), 1);
        assert!(watcher.is_registered(&ka));
        assert!(watcher.is_registered(&kb));

        // Removing one child keeps the parent watch (the other still needs it).
        watcher.unregister(&ka);
        assert!(!watcher.is_registered(&ka));
        assert_eq!(watcher.watched_parent_count(), 1);

        // Removing the last child drops the parent watch.
        watcher.unregister(&kb);
        assert_eq!(watcher.watched_parent_count(), 0);

        // Idempotent: a double-unregister is a no-op.
        watcher.unregister(&kb);
        assert_eq!(watcher.watched_parent_count(), 0);
    }

    #[test]
    fn deletion_emits_contributor_event() {
        let (watcher, mut rx) = WorktreeWatcher::new().expect("create watcher");
        let parent = tempfile::tempdir().expect("tempdir");
        let wt = parent.path().join("agent-del");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        let key = format!("worktree:s1:{}", wt.display());

        assert!(watcher.register(&key, &wt));
        std::fs::remove_dir_all(&wt).expect("rm wt");

        // The OS watch is async; poll the channel briefly for the reap event.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut got: Option<WorktreeDeleted> = None;
        while std::time::Instant::now() < deadline {
            if let Ok(ev) = rx.try_recv()
                && ev.contributor == key
            {
                got = Some(ev);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(
            got.is_some(),
            "expected a deletion event for the watched worktree within the deadline",
        );
    }

    #[test]
    fn no_deletion_emits_no_event() {
        // SendMessage-no-reap (ticket 05): the watch fires only on a `Remove`, so
        // a resume that reuses the worktree without deleting its dir reaps
        // nothing. A registered watch with no deletion produces no event within a
        // bounded poll — the inverse of `deletion_emits_contributor_event`. This
        // is non-flaky: only a spurious `Remove` could fail it, and the watcher
        // emits none without an actual deletion. The dir is touched (a file is
        // created) to confirm non-delete activity under the watched parent does
        // not reap.
        let (watcher, mut rx) = WorktreeWatcher::new().expect("create watcher");
        let parent = tempfile::tempdir().expect("tempdir");
        let wt = parent.path().join("agent-live");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        let key = format!("worktree:s1:{}", wt.display());

        assert!(watcher.register(&key, &wt));

        // Non-delete activity under the watched parent: create a sibling file and
        // a file inside the worktree. A `SendMessage` resume reuses the worktree
        // exactly like this — no dir deletion.
        std::fs::write(parent.path().join("note.txt"), b"resume").expect("write sibling");
        std::fs::write(wt.join("edited.rs"), b"fn main() {}").expect("write inside wt");

        // Poll briefly; collect any spurious event. A short bounded window is
        // enough — the watcher would have delivered a `Remove` within it (the
        // deletion test observes events in well under this budget), and there is
        // no deletion to deliver.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut spurious: Option<WorktreeDeleted> = None;
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ev) => {
                    spurious = Some(ev);
                    break;
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(
            spurious.is_none(),
            "no reap event should arrive without a deletion: {spurious:?}",
        );
        assert!(
            watcher.is_registered(&key),
            "the watch is still live (nothing was reaped)",
        );
    }

    #[test]
    fn unregister_after_live_deletion_does_not_deadlock() {
        // Regression (ticket 05): `release_parent`/`unregister` must NOT hold the
        // `state` lock across `notify`'s blocking `unwatch` — `unwatch` waits on the
        // watcher callback thread, which locks `state`, so holding it deadlocks.
        // Deleting a live-watched dir fires that callback; unregistering
        // concurrently must still complete. Pre-fix this hung indefinitely.
        let (watcher, _rx) = WorktreeWatcher::new().expect("create watcher");
        let parent = tempfile::tempdir().expect("tempdir");
        let wt = parent.path().join("agent-live");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        let key = format!("worktree:s1:{}", wt.display());
        assert!(watcher.register(&key, &wt));

        // Fire the OS deletion event into the callback, then unregister on another
        // thread — the previous (unwatch-while-holding-`state`) code deadlocked here.
        std::fs::remove_dir_all(&wt).expect("rm wt");
        let unregisterer = watcher.clone();
        let probe = key.clone();
        let handle = std::thread::spawn(move || unregisterer.unregister(&probe));

        // Require completion within a bounded window; a regression re-deadlocks and
        // trips the deadline instead of hanging the whole test binary.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !handle.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "unregister deadlocked after a live-watched deletion",
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        handle.join().expect("unregister thread panicked");
        assert!(!watcher.is_registered(&key));
    }
}
