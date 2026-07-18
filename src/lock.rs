// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The durable root lock — one cook per kitchen (root-ownership stage 2).
//!
//! Multiple editors in a single root is a breakage of the model: two agents
//! editing one root produce diagnostics debt that cannot be assigned and
//! receipts showing work the reader did not author. This module is the durable,
//! on-disk mutual-exclusion mechanism that enforces the rule at the edit seam —
//! **hook-process-local filesystem operations that work with the daemon down**.
//! Read-only agents (grep/glob/read) are never bounded by ownership; only the
//! edit seam acquires.
//!
//! # Anatomy
//!
//! One directory per root at `state_dir/locks/<encoded-root>/` (the same
//! root-encoding the JSONL firehose shard keys use, [`crate::paths::encode_cwd`]).
//! The **directory is the mutual-exclusion unit**: `mkdir` claims the ROOT, not
//! a `(root, owner)` pair, so two cooks on different owner names cannot both
//! succeed. Inside:
//!
//! - an **owner-identity file** whose *name* is the full identity tuple
//!   `<client>+<session>+<agent>` (client included because session ids are
//!   client-scoped opaque strings). All owner-file mutations are atomic
//!   rename-over — the same idiom config mutation uses (bug 109). Its **mtime**
//!   is the last-activity clock.
//! - `dir/`, a mirrored tree of empty **touch files** `<relpath>.lock`, one per
//!   due file. The `.lock` suffix is deliberate: language servers demonstrably
//!   index agent-explored state dirs, and ledger entries must not read as source.
//! - `.root`, the **root record** — the canonical root path in full (the dir
//!   name's encoding is lossy), written at first booking so the bare-serve
//!   enumeration can recover every indebted kitchen without a cwd to key from
//!   (bug 121, [`debtor_roots_in`]).
//!
//! # Lifecycle
//!
//! Acquisition rides the edit seam (see [`acquire_in`]). Booking is
//! static-data-driven at the hook — a file books iff its extension/filename maps
//! to a configured-or-blessed server or linter in the static config
//! ([`Booking`]); NO daemon connection, ever. Delivery (`catenary diagnostics`)
//! deletes touch files daemon-side ([`unlink_delivered_in`]) — empty `dir/` =
//! paid = the idle countdown starts. **Payment is parole, not release**: the lock
//! survives payment so the kitchen is not unlocked between a receipt and the next
//! edit of an actively-working agent.
//!
//! Only two legs release a lock: the daemon's paid-idle countdown
//! ([`reap_paid_idle_locks_in`]) and root retirement ([`retire_in`]). Staleness
//! auto-release, conversation-lifecycle release (Stop/SessionEnd), and any
//! daemon-death leg are explicitly rejected — a lock survives daemon churn and
//! reboots by construction.
//!
//! # Testability
//!
//! Every filesystem-facing function takes an explicit `locks_base: &Path` so a
//! test can point it at a tempdir without mutating the process environment
//! (`std::env::set_var` is forbidden under Rust 2024). The thin `*_in`-less
//! production wrappers resolve the base through [`locks_dir`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The identity tuple that titles the owner file: `<client>+<session>+<agent>`.
///
/// This is the one seam where identity appears — the `PreToolUse` hook, where
/// the host supplies it. The client is included because session ids are
/// client-scoped opaque strings, so a bare `session+agent` could collide across
/// hosts. Below the hook nothing uses identity; the CLI resolves by path algebra.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    /// Declared client (host format: `claude` / `antigravity` / `opencode`).
    pub client: String,
    /// Host session id (client-scoped opaque string). Empty when the host
    /// supplied none.
    pub session: String,
    /// Agent id. Empty string is the main (parent) agent.
    pub agent: String,
}

/// Separator between the three identity components in the owner file name.
///
/// `+` is filesystem-safe on every platform and never appears in the fixed
/// client tokens, so the join is unambiguous. Parsing splits on the first two
/// separators, so a `+` inside a session or agent id round-trips whole.
const OWNER_SEP: char = '+';

impl Owner {
    /// Builds an owner from the hook's identity fields.
    #[must_use]
    pub fn new(
        client: impl Into<String>,
        session: impl Into<String>,
        agent: impl Into<String>,
    ) -> Self {
        Self {
            client: client.into(),
            session: session.into(),
            agent: agent.into(),
        }
    }

    /// The owner file's name: `<client>+<session>+<agent>`.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!(
            "{}{OWNER_SEP}{}{OWNER_SEP}{}",
            self.client, self.session, self.agent
        )
    }

    /// Parses an owner from a file name produced by [`Self::file_name`].
    ///
    /// Splits on the first two `+` separators, so a session or agent id carrying
    /// a literal `+` survives whole. Returns `None` for a name with fewer than
    /// two separators (not an owner file this scheme produced).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        let (client, rest) = name.split_once(OWNER_SEP)?;
        let (session, agent) = rest.split_once(OWNER_SEP)?;
        Some(Self {
            client: client.to_string(),
            session: session.to_string(),
            agent: agent.to_string(),
        })
    }
}

/// Outcome of an [`acquire_in`] call at the edit seam.
#[derive(Debug)]
pub enum Acquired {
    /// The edit is admitted — this owner holds the lock (freshly claimed,
    /// re-affirmed, or the edit is in genuinely-foreign territory with no lock).
    Ours,
    /// The root is held by another agent; the edit is denied. Carries the
    /// briefing to show the entering second cook.
    Denied(String),
}

/// Outcome of a [`claim_in`] takeover attempt.
#[derive(Debug)]
pub enum Claimed {
    /// The lock was transferred to the claimant. Carries the prior owner's
    /// last-activity age (evidence) and the inherited due count.
    Ok {
        /// The previous editor's last-activity age at the moment of the claim.
        previous_age: Option<Duration>,
        /// Files inherited on the rail — the previous owner's outstanding debt.
        due: usize,
        /// Whether the inherited lock was paid and inside its idle window (the
        /// softened-copy signal).
        paid_and_idle: bool,
    },
    /// The root holds no lock — nothing to claim.
    Unlocked,
    /// The claimant already holds this lock — a no-op takeover.
    AlreadyOurs,
}

/// Root directory that holds every per-root lock: `state_dir/locks/`.
#[must_use]
pub fn locks_dir() -> PathBuf {
    crate::paths::state_dir().join("locks")
}

/// The lock directory for a single root under `locks_base`:
/// `<locks_base>/<encoded-root>/`.
///
/// Uses [`crate::paths::encode_cwd`] — the same flattening the firehose shard
/// keys use. The encoding is lossy but stable; a collision between two distinct
/// roots is astronomically unlikely for real project paths and, were it to
/// happen, degrades to a shared lock (safe: it only over-excludes). For a
/// near-`PATH_MAX` component the mirrored relpaths under `dir/` inherit the
/// state-dir prefix, so the touch-tree stays well within limits.
#[must_use]
pub fn root_lock_dir_in(locks_base: &Path, root: &Path) -> PathBuf {
    locks_base.join(crate::paths::encode_cwd(root))
}

/// The lock directory for a single root under the production [`locks_dir`].
#[must_use]
pub fn root_lock_dir(root: &Path) -> PathBuf {
    root_lock_dir_in(&locks_dir(), root)
}

/// The `dir/` touch-tree base inside a root's lock directory.
fn ledger_dir(lock_dir: &Path) -> PathBuf {
    lock_dir.join("dir")
}

/// The takeover-breadcrumb marker file inside a root's lock directory
/// (root-ownership stage 3, deliverable 7).
///
/// Written by [`claim_in`] on a successful takeover, read-and-removed by the
/// first diagnose serve after the claim ([`take_claim_marker_in`]). Its presence
/// is the sole signal that the root was claimed from a prior editor since the
/// last serve, so the receipt can lead with the claimed line. The `.claimed`
/// name is filesystem-safe, never owner-shaped (no `+` separators, so
/// [`read_owner_name`] skips it), and lives beside `dir/` — never inside the
/// touch-tree, so it is not miscounted as a due file.
fn claim_marker(lock_dir: &Path) -> PathBuf {
    lock_dir.join(".claimed")
}

/// The root-record file inside a root's lock directory (bug 121).
///
/// The lock dir's NAME is [`crate::paths::encode_cwd`]-flattened — lossy, so
/// the root path cannot be recovered from it. The record carries the canonical
/// root path itself, letting the bare-serve enumeration ([`debtor_roots_in`])
/// recover every kitchen whose ledger holds debt without a cwd to key from.
/// Like `.claimed`, the name is filesystem-safe and never owner-shaped (no `+`
/// separators, so [`read_owner_name`] skips it), and it lives beside `dir/` —
/// never inside the touch-tree, so it is not miscounted as a due file.
fn root_record(lock_dir: &Path) -> PathBuf {
    lock_dir.join(".root")
}

/// Writes the canonical root path into the lock dir's root record, once.
///
/// Rename-over (the bug-109 idiom), so a reader sees the whole path or no
/// record — never a torn one. Write-if-absent: the record's content is a pure
/// function of the lock dir's own name (same encoding input), so re-booking
/// never needs to rewrite it. Best-effort: a failed write leaves the lock dir
/// exactly as before the fix — the root simply stays invisible to the
/// all-kitchens enumeration until a later booking lands the record.
fn record_root(lock_dir: &Path, root: &Path) {
    let record = root_record(lock_dir);
    if record.exists() {
        return;
    }
    let tmp = lock_dir.join(format!(".root.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, root.to_string_lossy().as_bytes()).is_ok()
        && std::fs::rename(&tmp, &record).is_err()
    {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Reads the lock dir's root record, or `None` when absent/unreadable (a lock
/// dir booked before the record existed, or a transient FS error).
fn read_root_record(lock_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(root_record(lock_dir)).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(PathBuf::from(text))
}

/// The touch-file path for a due file, mirrored under `dir/` with a `.lock`
/// suffix: `dir/<root-relative-path>.lock`.
///
/// When the file is not under `root` (defensive — the caller resolves and
/// canonicalizes the root first, so this should not happen in practice), the
/// name flattens to a single ledger-safe component via [`crate::paths::encode_cwd`]
/// rather than joining the raw absolute path: joining an absolute path REPLACES
/// the ledger base, which would drop `.lock` files into the workspace (misc 193).
fn touch_file(lock_dir: &Path, root: &Path, file: &Path) -> PathBuf {
    let mut name = file.strip_prefix(root).map_or_else(
        |_| std::ffi::OsString::from(crate::paths::encode_cwd(file)),
        |rel| rel.as_os_str().to_os_string(),
    );
    name.push(".lock");
    ledger_dir(lock_dir).join(name)
}

/// Reads the owner file's name from a lock directory, if one exists.
///
/// Returns `Ok(Some(name))` when exactly one owner-titled entry is present,
/// `Ok(None)` when the directory holds no owner file yet (an acquisition is
/// mid-swing this instant — the caller treats that as locked). `Err` on an
/// unreadable directory.
fn read_owner_name(lock_dir: &Path) -> std::io::Result<Option<String>> {
    let mut found = None;
    for entry in std::fs::read_dir(lock_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // The touch-tree lives under `dir/`; skip it and any staging temp.
        if name == "dir" || name.contains(".tmp.") {
            continue;
        }
        // First owner-shaped entry wins; the scheme only ever writes one.
        if Owner::parse(name).is_some() {
            found = Some(name.to_string());
            break;
        }
    }
    Ok(found)
}

/// Writes the owner file atomically (rename-over), replacing any prior owner
/// title in the same directory.
///
/// Mirrors the bug-109 config-mutation idiom: stage a sibling temp
/// (`<lock_dir>/.owner.tmp.<pid>`), write it, then rename it over the final
/// owner path. The rename is a same-directory atomic replace on POSIX, so a
/// crash or concurrent writer leaves either the old title or the new one, never
/// a torn name. Any pre-existing owner file (a claim's old title) is removed
/// first so exactly one title remains.
fn write_owner_atomic(lock_dir: &Path, owner: &Owner) -> std::io::Result<()> {
    // Remove any prior owner title so the directory holds exactly one.
    if let Ok(Some(prior)) = read_owner_name(lock_dir)
        && prior != owner.file_name()
    {
        let _ = std::fs::remove_file(lock_dir.join(&prior));
    }
    let final_path = lock_dir.join(owner.file_name());
    let tmp = lock_dir.join(format!(".owner.tmp.{}", std::process::id()));
    // An empty file: the *name* carries the identity, the *mtime* the activity.
    std::fs::write(&tmp, b"")?;
    if let Err(e) = std::fs::rename(&tmp, &final_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Refreshes the owner file's mtime to now — the last-activity heartbeat.
///
/// Best-effort: a failure to bump the clock never blocks the edit. Opening the
/// existing (already empty) file for write and re-setting its length re-stamps
/// its mtime without changing the name (the identity).
fn touch_owner(lock_dir: &Path, owner_name: &str) {
    let path = lock_dir.join(owner_name);
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_len(0));
}

/// Books a single due file into the ledger: touch `dir/<relpath>.lock`.
///
/// Idempotent — re-booking an already-booked file is a no-op create. Creates the
/// mirrored parent directories as needed. Best-effort: a booking failure never
/// blocks the edit (the gate still reads daemon-side debt this stage).
fn book_file(lock_dir: &Path, root: &Path, file: &Path) {
    // Every booking seam funnels through here, so a lock dir with debt always
    // carries its root record (bug 121) — the bare-serve enumeration can name
    // the kitchen without a cwd to key from.
    record_root(lock_dir, root);
    let touch = touch_file(lock_dir, root, file);
    if let Some(parent) = touch.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&touch);
}

/// The number of files awaiting diagnosis in a lock's ledger (touch-tree leaf
/// count).
///
/// A paid lock reports `0`; a lock with due files reports the count. Used by the
/// deny briefing and the claim answer. Best-effort: an unreadable ledger reports
/// `0`.
#[must_use]
pub fn due_count(lock_dir: &Path) -> usize {
    fn count_dir(dir: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let mut total = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                total += count_dir(&path);
            } else if path.extension().is_some_and(|e| e == "lock") {
                total += 1;
            }
        }
        total
    }
    count_dir(&ledger_dir(lock_dir))
}

/// The absolute paths of every file awaiting diagnosis in a root's ledger — the
/// due set the diagnose round computes over (root-ownership stage 3).
///
/// Walks the `dir/` touch-tree under `<locks_base>/<encoded-root>/`, strips each
/// `.lock` leaf's suffix, and rejoins it onto `root` to reconstruct the absolute
/// path the edit seam booked. The ledger is now the single source of truth for
/// the batch: a bare `catenary diagnostics` diagnoses exactly this set. Sorted
/// for a stable receipt order. Best-effort: an unreadable / absent ledger yields
/// an empty set (no debt).
#[must_use]
pub fn due_files_in(locks_base: &Path, root: &Path) -> Vec<PathBuf> {
    let lock_dir = root_lock_dir_in(locks_base, root);
    let base = ledger_dir(&lock_dir);
    let mut out = Vec::new();
    collect_due(&base, &base, root, &mut out);
    out.sort();
    out
}

/// Recursive touch-tree walker for [`due_files_in`]: for each `<relpath>.lock`
/// leaf under `base`, rejoins `<relpath>` (the path relative to `base`, minus the
/// `.lock` suffix) onto `root`.
fn collect_due(base: &Path, dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_due(base, &path, root, out);
        } else if path.extension().is_some_and(|e| e == "lock")
            && let Ok(rel) = path.strip_prefix(base)
        {
            // Drop the `.lock` suffix from the leaf, keeping the mirrored parents.
            let stem = rel.with_extension("");
            out.push(root.join(stem));
        }
    }
}

/// Production wrapper for [`due_files_in`] resolving the base through [`locks_dir`].
#[must_use]
pub fn due_files(root: &Path) -> Vec<PathBuf> {
    due_files_in(&locks_dir(), root)
}

/// Whether a root's ledger holds any unpaid debt — the gate's "is there
/// undelivered debt?" question, answered by the ledger (root-ownership stage 3).
///
/// A non-empty touch-tree means at least one edited file has not been diagnosed
/// since its last edit. The lock dir surviving with an empty `dir/` (paid,
/// inside its idle window) reports `false`. Best-effort: an unreadable ledger
/// reports `false` (never false-gate on a transient FS error).
#[must_use]
pub fn has_debt_in(locks_base: &Path, root: &Path) -> bool {
    due_count(&root_lock_dir_in(locks_base, root)) > 0
}

/// Production wrapper for [`has_debt_in`] resolving the base through [`locks_dir`].
#[must_use]
pub fn has_debt(root: &Path) -> bool {
    has_debt_in(&locks_dir(), root)
}

/// Every root whose ledger holds unpaid debt under `locks_base`, recovered from
/// each lock dir's root record (bug 121).
///
/// The all-kitchens answer the bare serve's enumeration needs: the lock-dir
/// name encoding is lossy, so this reads the `.root` record a booking wrote.
/// Each recovered path is self-checked — its encoding must reproduce the lock
/// dir's own name (a corrupted or collided record is skipped) and the root must
/// still exist as a directory (a vanished kitchen's leftovers are the retire /
/// reap legs' business, not a serve's). A lock dir booked before the record
/// existed has no record and is skipped — invisible exactly as it was pre-fix,
/// and self-healing: the next booking lands the record. Sorted for stable
/// output. Best-effort: an unreadable `locks_base` yields an empty list.
#[must_use]
pub fn debtor_roots_in(locks_base: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(locks_base) else {
        return out;
    };
    for entry in entries.flatten() {
        let lock_dir = entry.path();
        if !lock_dir.is_dir() || due_count(&lock_dir) == 0 {
            continue;
        }
        let Some(root) = read_root_record(&lock_dir) else {
            continue;
        };
        let record_matches_dir = lock_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name == crate::paths::encode_cwd(&root));
        if record_matches_dir && root.is_dir() {
            out.push(root);
        }
    }
    out.sort();
    out
}

/// Production wrapper for [`debtor_roots_in`] resolving the base through
/// [`locks_dir`].
#[must_use]
pub fn debtor_roots() -> Vec<PathBuf> {
    debtor_roots_in(&locks_dir())
}

/// The kitchens a **bare** `catenary diagnostics` from `cwd` serves — the cwd's
/// enclosing lock root plus every other debt-holding root attributable to the
/// same identity (bug 121).
///
/// The ledger, not the cwd, is the truth: the edit seam books each file into
/// its OWN resolved root, so a session's debt can span kitchens the caller is
/// not standing in. The serve path is identity-free by ruling (identity lives
/// at the hook), so attribution here is pure filesystem fact — the owner FILES
/// the edit seam titled:
///
/// - **Anchored** — the cwd's root holds a lock: serve that root plus every
///   debtor root titled with the SAME owner tuple. Another identity's kitchen
///   is never pulled.
/// - **Unanchored** (cwd root unlocked, or cwd outside any root): when every
///   debtor root shares ONE owner, attribution is unambiguous — serve them
///   (the honest answer for "cwd outside any root with debt elsewhere").
///   When multiple identities hold debt, no path-algebraic fact says which is
///   the caller's, so no extra kitchen is pulled — the serve degrades to the
///   cwd root's own ledger, exactly the pre-fix contract. Ownerless debtor
///   dirs (a crashed acquisition) are never attributed.
///
/// The hook-side owner gate vets every root this returns against the caller's
/// real identity before the CLI runs, so a foreign kitchen in the would-serve
/// set denies there rather than serving here (hookless boxes keep their
/// existing ungated posture). The cwd root, when resolvable, is always first —
/// and included regardless of debt, preserving the single-root contract (an
/// empty ledger answers `[no edited files]`).
#[must_use]
pub fn bare_serve_roots_in(locks_base: &Path, cwd: &Path) -> Vec<PathBuf> {
    let cwd_root = resolve_lock_root(cwd);
    let debtors: Vec<PathBuf> = debtor_roots_in(locks_base)
        .into_iter()
        .filter(|r| Some(r) != cwd_root.as_ref())
        .collect();
    let anchor = cwd_root
        .as_deref()
        .and_then(|root| owner_of_in(locks_base, root));
    let extras: Vec<PathBuf> = if let Some(anchor) = anchor {
        debtors
            .into_iter()
            .filter(|root| owner_of_in(locks_base, root).is_some_and(|o| o == anchor))
            .collect()
    } else {
        let owned: Vec<(Owner, PathBuf)> = debtors
            .into_iter()
            .filter_map(|root| owner_of_in(locks_base, &root).map(|o| (o, root)))
            .collect();
        let unambiguous = !owned.is_empty() && owned.windows(2).all(|pair| pair[0].0 == pair[1].0);
        if unambiguous {
            owned.into_iter().map(|(_, root)| root).collect()
        } else {
            Vec::new()
        }
    };
    cwd_root.into_iter().chain(extras).collect()
}

/// Production wrapper for [`bare_serve_roots_in`] resolving the base through
/// [`locks_dir`].
#[must_use]
pub fn bare_serve_roots(cwd: &Path) -> Vec<PathBuf> {
    bare_serve_roots_in(&locks_dir(), cwd)
}

/// Owner-vetted analogue of [`bare_serve_roots_in`] used by the hook (bugs
/// 124/128).
///
/// The hook is the one seam with caller identity. It calls this function to
/// compute the **vetted serve set** — the caller's cwd root plus every debtor
/// root whose lock is held by the same `owner` tuple — and deposits the result
/// in the daemon before `catenary diagnostics` runs. The identity-free daemon
/// serve consumes the deposit, bypassing the ambiguous `bare_serve_roots`
/// enumeration.
///
/// Unlike [`bare_serve_roots_in`]:
/// - The unanchored arm has no single-owner restriction: even when multiple
///   identities hold debt, the caller's own kitchens are always found (because
///   identity is available here).
/// - Foreign-owned extras are **excluded** rather than triggering a deny; the
///   deny is issued by the hook caller for the cwd's own root only.
///
/// The cwd root is always included first (single-root contract), even when
/// unresolvable (empty ledger → `[no edited files]`).
#[must_use]
pub fn vetted_serve_roots_in(locks_base: &Path, cwd: &Path, owner: &Owner) -> Vec<PathBuf> {
    let cwd_root = resolve_lock_root(cwd);
    // Collect extras before consuming `cwd_root` in the chain (releasing the
    // borrow the filter closure holds on `cwd_root.as_ref()`).
    let extras: Vec<PathBuf> = debtor_roots_in(locks_base)
        .into_iter()
        .filter(|r| Some(r) != cwd_root.as_ref())
        .filter(|root| owner_of_in(locks_base, root).is_some_and(|o| o == *owner))
        .collect();
    cwd_root.into_iter().chain(extras).collect()
}

/// Production wrapper for [`vetted_serve_roots_in`] resolving the base through
/// [`locks_dir`].
#[must_use]
pub fn vetted_serve_roots(cwd: &Path, owner: &Owner) -> Vec<PathBuf> {
    vetted_serve_roots_in(&locks_dir(), cwd, owner)
}

/// The subset of `candidates` still DUE (undiagnosed) on their roots' ledgers
/// under `locks_base` (bug 116).
///
/// This is the Stop-block's "which of these edited files are unpaid?" question,
/// answered by the durable ledger.
/// The Stop hook holds the in-memory batch's file list as its CANDIDATE set — it
/// names the paths the agent edited, but no longer gates on the in-memory
/// `delivered` flags (nothing in production pays them since the identity-free
/// diagnose serve landed, root-ownership stage 3). This re-keys the gate to the
/// single source of truth: for each candidate, resolve its root
/// ([`resolve_lock_root`]) and test membership in that root's due set
/// ([`due_files_in`]). A candidate still on the touch-tree is unpaid → returned;
/// a paid (unlinked), never-booked, or foreign-territory candidate is not.
///
/// **Path-spelling discipline (load-bearing):** every candidate is canonicalized
/// at this ingestion seam so its spelling matches the canonical `.lock` leaves the
/// edit seam booked and the canonical `due_files_in` reconstructs — a
/// symlinked-prefix alias must resolve to the SAME ledger answer, or the block
/// would split from the debt. Pin: [`due_candidates_read_through_aliased_spelling`].
///
/// Roots are read once and cached, so a batch spanning one kitchen reads its
/// ledger a single time. Preserves input order (minus the paid entries) for a
/// stable block message. Best-effort throughout: an unreadable ledger yields no
/// debt for that root (never a false block on a transient FS error).
#[must_use]
pub fn due_candidates_in(locks_base: &Path, candidates: &[PathBuf]) -> Vec<PathBuf> {
    use std::collections::HashMap;
    // One due-set read per distinct root, cached across the candidate scan.
    let mut due_by_root: HashMap<PathBuf, std::collections::BTreeSet<PathBuf>> = HashMap::new();
    let mut out = Vec::new();
    for candidate in candidates {
        // Canonicalize at the seam so the spelling matches the canonical ledger
        // (misc 193 / bug 116). A path that cannot canonicalize (a vanished
        // edit) keeps its spelling — it will simply miss the canonical due set,
        // which is the honest reading (a deleted file books no live debt).
        let file = candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.clone());
        let Some(root) = resolve_lock_root(&file) else {
            // Foreign territory (/tmp, scratch) — never booked, never due.
            continue;
        };
        let due = due_by_root
            .entry(root.clone())
            .or_insert_with(|| due_files_in(locks_base, &root).into_iter().collect());
        if due.contains(&file) {
            out.push(file);
        }
    }
    out
}

/// Production wrapper for [`due_candidates_in`] resolving the base through
/// [`locks_dir`].
#[must_use]
pub fn due_candidates(candidates: &[PathBuf]) -> Vec<PathBuf> {
    due_candidates_in(&locks_dir(), candidates)
}

/// The current owner of a root's lock, or `None` when the root is unlocked or
/// the lock dir is ownerless (root-ownership stage 3).
///
/// The hook-side diagnostics gate (deliverable 4) reads this to answer "does the
/// caller hold this root?" before allowing a bare `catenary diagnostics` to pull
/// its ledger. Parsing the owner file name recovers the `<client>+<session>+
/// <agent>` tuple; an absent owner file (unlocked, or a mid-swing / crashed
/// acquisition) yields `None` — an ungated serve, since there is no established
/// holder to protect.
#[must_use]
pub fn owner_of_in(locks_base: &Path, root: &Path) -> Option<Owner> {
    let lock_dir = root_lock_dir_in(locks_base, root);
    read_owner_name(&lock_dir)
        .ok()
        .flatten()
        .and_then(|name| Owner::parse(&name))
}

/// Production wrapper for [`owner_of_in`] resolving the base through [`locks_dir`].
#[must_use]
pub fn owner_of(root: &Path) -> Option<Owner> {
    owner_of_in(&locks_dir(), root)
}

/// Reads-and-removes the takeover breadcrumb for a root, returning `true` when
/// one was present (root-ownership stage 3, deliverable 7).
///
/// The first diagnose serve after a `catenary claim` calls this: a `true` result
/// leads the receipt with "root claimed from a prior editor", and the removal
/// makes it one-shot — a later serve on the same root (no new claim) sees no
/// marker. Best-effort: an absent marker or a removal failure yields `false` (the
/// breadcrumb is a nicety, never a gate).
#[must_use]
pub fn take_claim_marker_in(locks_base: &Path, root: &Path) -> bool {
    let marker = claim_marker(&root_lock_dir_in(locks_base, root));
    if marker.exists() {
        let _ = std::fs::remove_file(&marker);
        true
    } else {
        false
    }
}

/// Production wrapper for [`take_claim_marker_in`] resolving the base through
/// [`locks_dir`].
#[must_use]
pub fn take_claim_marker(root: &Path) -> bool {
    take_claim_marker_in(&locks_dir(), root)
}

/// How long ago the lock last saw activity, or `None` when no mtime is readable
/// or the clock reads in the future.
///
/// The owner file's mtime is the activity heartbeat. When the owner file is
/// **absent** — an acquisition mid-swing this instant, or one whose hook process
/// died between `mkdir` and the owner write — the lock DIR's own mtime stands in:
/// a genuinely mid-swing dir is milliseconds old (the mid-swing deny semantics
/// are untouched), while a crashed acquisition ages past the idle window so the
/// reaper clears it on a normal sweep instead of holding the root hostage
/// forever (the nag-never-hostage invariant).
#[must_use]
pub fn last_activity_age(lock_dir: &Path, now: SystemTime) -> Option<Duration> {
    let mtime = match read_owner_name(lock_dir).ok().flatten() {
        Some(owner) => std::fs::metadata(lock_dir.join(owner))
            .ok()?
            .modified()
            .ok()?,
        // Ownerless: the dir's own mtime is the acquisition instant (nothing
        // inside an ownerless dir ever changes, so it never refreshes).
        None => std::fs::metadata(lock_dir).ok()?.modified().ok()?,
    };
    now.duration_since(mtime).ok()
}

/// A read-only snapshot of a root's lock state — the filesystem facts the board
/// renders (root-ownership stage 6, deliverable 1).
///
/// Built by [`facts_for_in`] from the lock dir alone: the owner **label** (read
/// from the owner-file name — a display label, never a routing key), the due
/// count (the touch-tree leaf count), and the last-activity age (the owner
/// file's mtime against `now`). This is the whole read surface the board needs,
/// so the TUI never re-derives lock internals. Best-effort: an unreadable /
/// absent lock dir yields `None`, so the board renders such a root as unlocked
/// rather than erroring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockFacts {
    /// The owner label, `<client>+<session>+<agent>` (a display label read from
    /// the owner-file name). `None` for an ownerless lock dir (a crashed or
    /// mid-swing acquisition).
    pub owner: Option<Owner>,
    /// Files awaiting diagnosis in this root's ledger (the touch-tree leaf
    /// count). `0` for a paid lock inside its idle window.
    pub due: usize,
    /// How long ago the lock last saw activity, or `None` when no mtime is
    /// readable.
    pub age: Option<Duration>,
}

/// Reads the lock facts for a single `root` under `locks_base`, or `None` when
/// the root holds no lock dir (root-ownership stage 6, deliverable 1).
///
/// A pure filesystem read keyed by the root's canonical path — no daemon
/// contact — so the board's lock facts are indifferent to daemon lifecycle:
/// killing and restarting the daemon changes nothing here, since every field
/// comes from the durable lock dir. Best-effort: an unreadable owner name or
/// ledger degrades a field to its empty reading ([`None`] owner / `0` due /
/// [`None`] age) rather than erroring.
#[must_use]
pub fn facts_for_in(locks_base: &Path, root: &Path, now: SystemTime) -> Option<LockFacts> {
    let lock_dir = root_lock_dir_in(locks_base, root);
    if !lock_dir.is_dir() {
        return None;
    }
    Some(LockFacts {
        owner: read_owner_name(&lock_dir)
            .ok()
            .flatten()
            .and_then(|name| Owner::parse(&name)),
        due: due_count(&lock_dir),
        age: last_activity_age(&lock_dir, now),
    })
}

/// Production wrapper for [`facts_for_in`] resolving the base through [`locks_dir`].
///
/// The `root` is canonicalized at this ingestion seam (the spelling rule): the
/// board queries with the snapshot's stored root path, and a symlinked-prefix
/// alias must read the SAME canonical lock dir the edit seam booked under, or a
/// held root would render as unlocked. Pin: [`facts_read_through_aliased_spelling`].
#[must_use]
pub fn facts_for(root: &Path, now: SystemTime) -> Option<LockFacts> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    facts_for_in(&locks_dir(), &root, now)
}

/// Enumerates every lock dir under `locks_base`, returning each root's encoded
/// dir name paired with its facts (root-ownership stage 6, deliverable 1).
///
/// A read-only sweep of the lock dirs — the board's "what roots are locked
/// right now?" answer, keyed off the filesystem rather than daemon memory. The
/// encoding ([`crate::paths::encode_cwd`]) is lossy, so this cannot reconstruct
/// the absolute root path; the board pairs the encoded name against a known root
/// via [`root_lock_dir_in`] (or reads facts per-root through [`facts_for`]). Used
/// where the caller wants the full lock inventory (a daemon-down board that has
/// no root list to key against). Best-effort: an unreadable `locks_base` yields
/// an empty list.
#[must_use]
pub fn list_locks_in(locks_base: &Path, now: SystemTime) -> Vec<(String, LockFacts)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(locks_base) else {
        return out;
    };
    for entry in entries.flatten() {
        let lock_dir = entry.path();
        if !lock_dir.is_dir() {
            continue;
        }
        let Some(name) = lock_dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        out.push((
            name.to_string(),
            LockFacts {
                owner: read_owner_name(&lock_dir)
                    .ok()
                    .flatten()
                    .and_then(|n| Owner::parse(&n)),
                due: due_count(&lock_dir),
                age: last_activity_age(&lock_dir, now),
            },
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Production wrapper for [`list_locks_in`] resolving the base through [`locks_dir`].
#[must_use]
pub fn list_locks(now: SystemTime) -> Vec<(String, LockFacts)> {
    list_locks_in(&locks_dir(), now)
}

/// Renders an owner as a compact display label — `<client>+<session>+<agent>`.
///
/// This is the board's owner LABEL (design: labels are OK, keys are not). It is
/// the owner-file name verbatim, so it round-trips with the lock dir's title and
/// never invents a correlation the plane invariant forbids.
#[must_use]
pub fn owner_label(owner: &Owner) -> String {
    owner.file_name()
}

/// Resolves the root a file belongs to for locking purposes, or `None` when the
/// file is in genuinely-foreign territory (no repository marker above it).
///
/// Rides the same gate the query auto-mount uses
/// ([`crate::companions::enclosing_worktree_root`], repo-marker root
/// resolution): edits in `/tmp`, scratch dirs, or anywhere outside a VCS
/// checkout resolve to `None`, so no lock is taken. The root is canonicalized so
/// its encoding matches the daemon's canonical roots.
///
/// **Innermost covered root wins** — the same resolution queries use
/// ([`crate::companions::enclosing_worktree_root`] stops at the *nearest*
/// enclosing marker). A nested inner root (its own `.git`/marker) is resolved,
/// not the outer repo, so an edit inside it books against the INNER kitchen's
/// ledger and the outer root's lock is untouched. The lock, the ledger, and the
/// briefing therefore all agree which kitchen a nested path belongs to. Pin:
/// [`edit_in_nested_inner_root_books_inner_not_outer`].
#[must_use]
pub fn resolve_lock_root(file: &Path) -> Option<PathBuf> {
    let canonical = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let root = crate::companions::enclosing_worktree_root(&canonical)?;
    Some(root.canonicalize().unwrap_or(root))
}

/// Whether a lock is paid (empty ledger) and inside its idle window — the
/// softened-copy signal.
fn is_paid_and_idle(lock_dir: &Path, now: SystemTime, idle_timeout: Duration) -> bool {
    due_count(lock_dir) == 0
        && last_activity_age(lock_dir, now).is_some_and(|age| age < idle_timeout)
}

/// The deny briefing for a `root` held by another identity, computed from its
/// on-disk lock state — the same briefing the edit-seam collision produces
/// (root-ownership stage 5).
///
/// Reads the lock dir's age / due count / paid-idle state and renders
/// [`deny_briefing`]. The caller resolves the root and confirms it is held by
/// another owner first; this only formats. Used by the stateful-tier gate at the
/// hook (a `build` / mutating-git / `chmod` command in another cook's kitchen) so
/// its denial matches the edit seam's exactly, without re-deriving the internals
/// hook-side.
#[must_use]
pub fn holder_briefing(root: &Path, now: SystemTime) -> String {
    let lock_dir = root_lock_dir(root);
    let age = last_activity_age(&lock_dir, now);
    let due = due_count(&lock_dir);
    let paid_and_idle = is_paid_and_idle(&lock_dir, now, PAID_IDLE_TIMEOUT);
    deny_briefing(root, age, due, paid_and_idle)
}

/// Formats the deny briefing shown to an entering second cook (copy is ruled).
///
/// The root path is on its own line, copy-pasteable, and the `catenary claim`
/// invocation names the same root. When the lock is paid (no due files) and
/// inside its idle window the copy softens — claiming a debt-free lock is cheap.
#[must_use]
pub fn deny_briefing(
    root: &Path,
    age: Option<Duration>,
    due: usize,
    paid_and_idle: bool,
) -> String {
    let root = root.display();
    let ago = age.map_or_else(|| "just now".to_string(), format_age);
    if paid_and_idle {
        format!(
            "root locked: {root}\n\
             held by another agent — last activity {ago}; no unpaid debt; lock expires shortly.\n\
             If the previous agent is not coming back, take over with:\n\
             \x20 catenary claim {root}\n\
             Claiming transfers the root and its diagnostic debt to you."
        )
    } else {
        let files = if due == 1 {
            "1 file awaiting diagnosis".to_string()
        } else {
            format!("{due} files awaiting diagnosis")
        };
        format!(
            "root locked: {root}\n\
             held by another agent — last activity {ago}; {files}.\n\
             If the previous agent is not coming back, take over with:\n\
             \x20 catenary claim {root}\n\
             Claiming transfers the root and its diagnostic debt to you."
        )
    }
}

/// Formats the deny briefing for an ownerless lock dir older than the mid-swing
/// grace — an interrupted acquisition, not a working agent.
///
/// "held by another agent … 0 files awaiting diagnosis" would be a lie here: no
/// owner was ever installed, so the truth is that an acquisition was interrupted.
/// Same shape as [`deny_briefing`] — the root on its own line, the claim
/// invocation copy-pasteable — so the rescue is one command away. The paid-idle
/// reaper also clears the dir after the idle window (by dir mtime), so even an
/// unclaimed one is never a permanent hostage.
fn ownerless_briefing(root: &Path, age: Option<Duration>) -> String {
    let root = root.display();
    let ago = age.map_or_else(|| "just now".to_string(), format_age);
    format!(
        "root locked: {root}\n\
         an interrupted lock acquisition left this root locked with no owner (created {ago}).\n\
         Take over with:\n\
         \x20 catenary claim {root}\n\
         Claiming transfers the root and its diagnostic debt to you."
    )
}

/// Renders an activity age as a compact human string (`43s`, `26m`, `3h`).
#[must_use]
pub fn format_age(age: Duration) -> String {
    let secs = age.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// The idle window a **paid** lock survives before the daemon countdown removes
/// it (release leg 1).
///
/// Matched to [`crate::router`]'s ephemeral-root idle timeout so the two idle
/// clocks in the daemon agree; a re-edit inside the window re-books under the
/// same owner and re-arms the lock with no new ceremony.
pub const PAID_IDLE_TIMEOUT: Duration = Duration::from_mins(7);

/// How long an ownerless lock dir is presumed to be a mid-swing acquisition.
///
/// An acquisition writes its owner file within milliseconds of the `mkdir`; a
/// dir still ownerless past this grace means the acquiring hook process died
/// between the two steps. Inside the grace an entrant is denied with the
/// ordinary mid-swing briefing (the race semantics stand); past it the deny
/// tells the truth — an interrupted acquisition — and points at `catenary
/// claim` as the rescue. Generous next to the milliseconds the real window
/// takes, tiny next to [`PAID_IDLE_TIMEOUT`].
const OWNERLESS_GRACE: Duration = Duration::from_secs(5);

/// Acquires (or re-affirms) the durable root lock for an edited `file` under
/// `locks_base`, booking it when [`Booking`] says its type is covered.
///
/// **Hook-process-local**: every filesystem op here runs in the short-lived hook
/// process, so the lock works with the daemon down.
///
/// The protocol (no retry choreography):
/// 1. Resolve the file's root ([`resolve_lock_root`]); genuinely-foreign
///    territory ⇒ [`Acquired::Ours`] with no lock.
/// 2. `mkdir <locks_base>/<encoded-root>/`. Success ⇒ first cook: write the
///    owner file, book the file, allow.
/// 3. `EEXIST` ⇒ read the owner file:
///    - OURS ⇒ book (idempotent), bump last-activity, allow.
///    - SOMEONE ELSE'S ⇒ deny with the briefing.
///    - EMPTY DIR (no owner file yet) ⇒ an acquisition is mid-swing this
///      instant; treat as locked with last-activity = now and deny.
///
/// A standalone uncovered edit into an *unlocked* root takes no lock at all —
/// the edit gate would not arm for it, so the lock must not either (booking
/// honesty). A covered edit into an already-locked-by-us root re-books; an
/// uncovered edit into our own locked root re-affirms without booking.
#[must_use]
pub fn acquire_in(
    locks_base: &Path,
    file: &Path,
    owner: &Owner,
    booking: &Booking,
    now: SystemTime,
) -> Acquired {
    // Canonicalize the edited path at the ingestion seam (misc 193): one spelling
    // everywhere below — `resolve_lock_root`, `booking.books`, and both
    // `book_file` sites — so a symlinked-prefix alias (macOS `$TMPDIR` →
    // `/private/var/…`, or any symlinked checkout) books into the SAME canonical
    // ledger it will be paid against. Since stage 3 makes the ledger the single
    // source of truth, this spelling rule is load-bearing: a split spelling
    // splits the debt.
    let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let file = file.as_path();
    let Some(root) = resolve_lock_root(file) else {
        // Foreign territory (/tmp, scratch) — never tracked, no lock.
        return Acquired::Ours;
    };
    let books = booking.books(file);
    let lock_dir = root_lock_dir_in(locks_base, &root);

    match std::fs::create_dir_all(locks_base).and_then(|()| std::fs::create_dir(&lock_dir)) {
        Ok(()) => {
            // First cook. A standalone uncovered edit into a fresh root takes no
            // lock — the gate would not arm for it, so the lock must not either
            // (booking honesty). Undo the just-made dir and allow.
            if !books {
                let _ = std::fs::remove_dir(&lock_dir);
                return Acquired::Ours;
            }
            let _ = write_owner_atomic(&lock_dir, owner);
            book_file(&lock_dir, &root, file);
            Acquired::Ours
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            match read_owner_name(&lock_dir) {
                Ok(Some(name)) if name == owner.file_name() => {
                    // Ours — book (idempotent), bump last-activity, allow.
                    if books {
                        book_file(&lock_dir, &root, file);
                    }
                    touch_owner(&lock_dir, &name);
                    Acquired::Ours
                }
                Ok(Some(_)) => {
                    // Someone else's — deny with the briefing.
                    let age = last_activity_age(&lock_dir, now);
                    let due = due_count(&lock_dir);
                    let paid_and_idle = is_paid_and_idle(&lock_dir, now, PAID_IDLE_TIMEOUT);
                    Acquired::Denied(deny_briefing(&root, age, due, paid_and_idle))
                }
                Ok(None) => {
                    // Ownerless dir. Two truths, told apart by age (the dir's
                    // own mtime via `last_activity_age`): inside the grace it is
                    // an acquisition mid-swing this instant — treat as locked
                    // with last-activity = now and deny (no retry); past the
                    // grace the acquiring hook died between `mkdir` and the
                    // owner write, so the briefing says what is true and points
                    // at `catenary claim` as the rescue (the paid-idle reaper
                    // also clears it after the window). Never a permanent
                    // hostage.
                    let age = last_activity_age(&lock_dir, now);
                    if age.is_some_and(|a| a > OWNERLESS_GRACE) {
                        Acquired::Denied(ownerless_briefing(&root, age))
                    } else {
                        Acquired::Denied(deny_briefing(&root, Some(Duration::ZERO), 0, false))
                    }
                }
                Err(_) => {
                    // Unreadable lock dir — fail open (never false-deny a lone
                    // agent on a transient FS error).
                    Acquired::Ours
                }
            }
        }
        Err(_) => {
            // mkdir failed for a reason other than EEXIST (permissions, etc.) —
            // fail open. The lock is best-effort; a lone agent must never be
            // false-denied by an FS hiccup.
            Acquired::Ours
        }
    }
}

/// Production wrapper for [`acquire_in`] resolving the base through [`locks_dir`].
#[must_use]
pub fn acquire(file: &Path, owner: &Owner, booking: &Booking, now: SystemTime) -> Acquired {
    acquire_in(&locks_dir(), file, owner, booking, now)
}

/// Claims a root's lock for `claimant` under `locks_base` — the agent-invocable
/// takeover (`catenary claim <root>`).
///
/// One atomic rename of the owner file to the claimant's tuple; the old→new
/// title pair is the audit record. The inherited ledger (the previous owner's
/// due files) rides along untouched — per-root inheritance ONLY, so the previous
/// owner's OTHER kitchens keep their rails. Recency is evidence, not a trigger:
/// there is no auto-release on staleness; the claimant judges.
///
/// An **ownerless** lock dir (a crashed acquisition — the hook died between
/// `mkdir` and the owner write) is claimable too: claim installs the claimant's
/// owner file (same atomic idiom) instead of failing, so the rescue is one
/// command and never waits on the reaper. That rescue plus the reaper's
/// dir-mtime countdown is what keeps a crashed acquisition from ever holding a
/// root hostage (the nag-never-hostage invariant).
///
/// Returns [`Claimed::Unlocked`] when the root holds no lock (nothing to take),
/// [`Claimed::AlreadyOurs`] when the claimant already holds it, else
/// [`Claimed::Ok`] with the prior owner's evidence.
#[must_use]
pub fn claim_in(locks_base: &Path, root: &Path, claimant: &Owner, now: SystemTime) -> Claimed {
    let lock_dir = root_lock_dir_in(locks_base, root);
    if !lock_dir.is_dir() {
        return Claimed::Unlocked;
    }
    match read_owner_name(&lock_dir) {
        Ok(Some(name)) if name == claimant.file_name() => Claimed::AlreadyOurs,
        Ok(Some(_) | None) => {
            let previous_age = last_activity_age(&lock_dir, now);
            let due = due_count(&lock_dir);
            let paid_and_idle = is_paid_and_idle(&lock_dir, now, PAID_IDLE_TIMEOUT);
            // The one atomic rename-over: the new title replaces the old, the
            // ledger stays put. A fresh mtime records the takeover instant.
            let _ = write_owner_atomic(&lock_dir, claimant);
            // Drop the takeover breadcrumb (root-ownership stage 3, deliverable
            // 7): the first diagnose serve after this claim reads-and-removes it
            // and leads its receipt with the claimed line. Best-effort — a failed
            // marker write only loses the one-time breadcrumb, never the takeover.
            let _ = std::fs::write(claim_marker(&lock_dir), b"");
            Claimed::Ok {
                previous_age,
                due,
                paid_and_idle,
            }
        }
        Err(_) => Claimed::Unlocked,
    }
}

/// Production wrapper for [`claim_in`] resolving the base through [`locks_dir`].
#[must_use]
pub fn claim(root: &Path, claimant: &Owner, now: SystemTime) -> Claimed {
    claim_in(&locks_dir(), root, claimant, now)
}

/// Renders the `catenary claim` answer for a successful takeover (copy is
/// ruled).
///
/// Catenary reports only what Catenary knows: the due count and the serve
/// command. Git owns "what changed" — the last line points at `git diff` /
/// `git status`, never a rendered diff. Softens when the inherited lock was paid
/// and inside its idle window ("no unpaid debt; lock expires shortly") —
/// claiming a debt-free lock is cheap.
#[must_use]
pub fn claim_answer(
    root: &Path,
    previous_age: Option<Duration>,
    due: usize,
    paid_and_idle: bool,
) -> String {
    let root = root.display();
    let seen = previous_age.map_or_else(|| "just now".to_string(), format_age);
    if paid_and_idle {
        format!(
            "claimed {root} (previous editor last seen {seen})\n\
             no unpaid debt; lock expires shortly.\n\
             Review the inherited edits with git diff / git status."
        )
    } else {
        let files = if due == 1 {
            "1 file awaiting diagnosis".to_string()
        } else {
            format!("{due} files awaiting diagnosis")
        };
        format!(
            "claimed {root} (previous editor last seen {seen})\n\
             {files} — run `catenary diagnostics` to serve them.\n\
             Review the inherited edits with git diff / git status."
        )
    }
}

/// The per-entry result of a delivery unlink — the forensic receipt the
/// delivery seam traces so a leaked ledger entry names WHERE the payment went
/// missing (bug 120).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlinkOutcome {
    /// The touch leaf existed and was removed — the entry is paid.
    Unlinked,
    /// No lock root resolves for the file (foreign territory) — there is no
    /// ledger to pay. Only produced by [`unlink_delivered_by_root`].
    NoRoot,
    /// The root has no lock directory at all — nothing was ever booked (or the
    /// lock was already retired/reaped).
    NoLedger,
    /// The lock directory exists but holds no touch leaf for this path — the
    /// file was served without standing debt (an on-demand scoped serve), or
    /// the delivery spelling diverged from the booking's.
    NoEntry,
    /// The leaf could not be removed (an I/O error other than not-found) — the
    /// entry survives as phantom debt.
    Failed(std::io::ErrorKind),
}

/// Unlinks the touch files for a set of delivered files from a root's ledger
/// under `locks_base` (delivery deletes — the daemon knows which files served).
///
/// Called daemon-side on the diagnostics-delivery seam. Removing a file's touch
/// entry marks it paid; when `dir/` empties, the lock is paid and the idle
/// countdown starts. **Does not remove the lock** — payment is parole, not
/// release. Prunes now-empty mirrored subdirectories so `dir/` genuinely empties.
/// Best-effort: a failed unlink never blocks the caller (the reaper's empty
/// check still holds) — but each entry's [`UnlinkOutcome`] is returned so the
/// delivery seam can trace exactly which entries paid, which were skipped, and
/// which failed (bug 120). Callers with no forensic seam ignore the receipt.
pub fn unlink_delivered_in(
    locks_base: &Path,
    root: &Path,
    files: &[PathBuf],
) -> Vec<(PathBuf, UnlinkOutcome)> {
    let lock_dir = root_lock_dir_in(locks_base, root);
    if !lock_dir.exists() {
        return files
            .iter()
            .map(|f| (f.clone(), UnlinkOutcome::NoLedger))
            .collect();
    }
    let base = ledger_dir(&lock_dir);
    let mut outcomes = Vec::with_capacity(files.len());
    for file in files {
        // Canonicalize the delivered file at the seam (misc 193): the touch path
        // must be computed from the same spelling `acquire_in` booked it under,
        // or a symlinked-prefix alias would compute a different `.lock` leaf and
        // fail to unlink — leaving phantom debt on a canonical ledger.
        let file = file.canonicalize().unwrap_or_else(|_| file.clone());
        let touch = touch_file(&lock_dir, root, &file);
        let outcome = match std::fs::remove_file(&touch) {
            Ok(()) => UnlinkOutcome::Unlinked,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => UnlinkOutcome::NoEntry,
            Err(e) => UnlinkOutcome::Failed(e.kind()),
        };
        outcomes.push((file, outcome));
        // Prune now-empty mirrored parents up to (but not including) `dir/`.
        let mut cur = touch.parent().map(Path::to_path_buf);
        while let Some(dir) = cur {
            if dir == base || !dir.starts_with(&base) {
                break;
            }
            if std::fs::remove_dir(&dir).is_err() {
                break; // non-empty or gone — stop pruning
            }
            cur = dir.parent().map(Path::to_path_buf);
        }
    }
    outcomes
}

/// Production wrapper for [`unlink_delivered_in`] resolving the base through
/// [`locks_dir`].
#[allow(
    clippy::must_use_candidate,
    reason = "called for the unlink side effect; the outcome receipt is optional forensics (bug 120)"
)]
pub fn unlink_delivered(root: &Path, files: &[PathBuf]) -> Vec<(PathBuf, UnlinkOutcome)> {
    unlink_delivered_in(&locks_dir(), root, files)
}

/// Books a set of files into `root`'s ledger under `owner`, creating the lock
/// dir + owner file if the root is not yet locked (root-ownership stage 3).
///
/// The bulk booking seam behind the merge bracket's debt transfer
/// ([`merge_transfer_in`], wf-01) and the Book-direction reconcile
/// ([`reconcile_bracket_in`]): a merged worktree's unpaid files become the
/// merging agent's debt, booked directly into the owning repo's ledger
/// (identity lives at the hook, forwarded here) so a later
/// `catenary diagnostics` there serves them. Distinct from the edit-seam
/// [`acquire_in`] — this is a bulk transfer, not a per-edit acquisition, so it
/// books unconditionally (the caller already resolved the transfer set) without
/// the booking/coverage gate. Each file is canonicalized so its touch leaf
/// matches the canonical ledger a later `due_files`/serve reads (misc 193).
/// Best-effort: a failed create never blocks the caller.
pub fn book_transferred_in(locks_base: &Path, root: &Path, owner: &Owner, files: &[PathBuf]) {
    if files.is_empty() {
        return;
    }
    let lock_dir = root_lock_dir_in(locks_base, root);
    // Ensure the lock dir and an owner file exist (the landing agent owns the
    // root's inherited debt). `create_dir_all` is idempotent; the owner write is
    // rename-over, so a re-titled owner on an already-locked root is fine.
    let _ = std::fs::create_dir_all(&lock_dir);
    if read_owner_name(&lock_dir).ok().flatten().is_none() {
        let _ = write_owner_atomic(&lock_dir, owner);
    }
    for file in files {
        let file = file.canonicalize().unwrap_or_else(|_| file.clone());
        book_file(&lock_dir, root, &file);
    }
}

/// The direction a reconcile bracket drives the ledger (root-ownership stage 5).
///
/// A stateful-tier git command is bracketed by a post-command reconcile with git
/// itself as the changed-ness oracle (`git status --porcelain`). Which way the
/// ledger moves is the command's effect on the working tree:
///
/// - [`Unbook`](ReconcileDirection::Unbook) — `git stash` / `git checkout` /
///   `git merge --abort` remove modifications, so files git now reports CLEAN
///   are unbooked (their debt left the working tree with the modifications).
/// - [`Book`](ReconcileDirection::Book) — `git stash pop` / `rebase` restore
///   the agent's OWN modifications, so every COVERED file git reports MODIFIED
///   is booked ("the pop should present as writes that restore it").
///
/// One bracket, one oracle, per command, both directions — nothing
/// stash-specific. `git merge` no longer drives a Book reconcile: debt means
/// "an agent edited this and nobody has looked," not "content moved" (the
/// pull-parity ruling, wf-01), so a merge only transfers unpaid worktree debt
/// via [`merge_transfer_in`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileDirection {
    /// Files git now reports clean are unbooked.
    Unbook,
    /// Covered files git reports modified are booked.
    Book,
}

/// Reconciles a root's ledger against git's changed-ness oracle after a
/// stateful-tier git command, in the given `direction` (root-ownership stage 5).
///
/// `modified` is the canonical absolute path set git reports as changed after the
/// command — the `git status --porcelain` result, joined onto the repo root and
/// canonicalized at its ingestion seam (the spelling rule: these paths key ledger
/// reads/writes, so they must carry the SAME canonical spelling the edit seam
/// booked under). The bracket derives both directions from that one oracle plus
/// the current due set:
///
/// - [`Unbook`](ReconcileDirection::Unbook): every currently-due file NOT in
///   `modified` went clean — the modification left the working tree, so its touch
///   file is unlinked ([`unlink_delivered_in`] is the existing unlink primitive).
///   Debt goes with the modifications.
/// - [`Book`](ReconcileDirection::Book): every file in `modified` that [`Booking`]
///   says is covered is booked (creating the lock dir + owner file if the root is
///   not yet locked, so a pop into a paid/expired kitchen re-arms it). The static
///   `Booking` gate is the SAME one the edit seam uses, so the pop never books an
///   uncoverable file. Over-booking an already-diagnosed file whose bytes the pop
///   restored is the safe direction — it re-diagnoses clean and retires;
///   under-booking would lose the debt the ruling restores.
///
/// **Attribution is clean** because the bracket wraps the cook's OWN command at
/// the hook — no watcher guesswork. Best-effort throughout: a booking/unlink
/// failure never blocks the command (it already ran).
pub fn reconcile_bracket_in(
    locks_base: &Path,
    root: &Path,
    owner: &Owner,
    booking: &Booking,
    direction: ReconcileDirection,
    modified: &[PathBuf],
) {
    // The oracle's spelling must match the ledger's canonical spelling — the
    // caller canonicalizes at the git-query seam, but re-canonicalize here so a
    // direct call (a test, a future caller) is never split by a symlinked prefix.
    let modified: std::collections::BTreeSet<PathBuf> = modified
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();

    match direction {
        ReconcileDirection::Unbook => {
            // Files that were due but git now reports clean left the working tree
            // — unbook them. A file still modified stays due (its debt is real).
            let cleared: Vec<PathBuf> = due_files_in(locks_base, root)
                .into_iter()
                .filter(|due| !modified.contains(due))
                .collect();
            if !cleared.is_empty() {
                unlink_delivered_in(locks_base, root, &cleared);
            }
        }
        ReconcileDirection::Book => {
            // Every covered modified file is booked — the pop presents as writes
            // that restore it. `Booking` is the same static gate the edit seam
            // uses, so an uncoverable file never books.
            let to_book: Vec<PathBuf> = modified
                .into_iter()
                .filter(|file| booking.books(file))
                .collect();
            if !to_book.is_empty() {
                book_transferred_in(locks_base, root, owner, &to_book);
            }
        }
    }
}

/// Production wrapper for [`reconcile_bracket_in`] resolving the base through
/// [`locks_dir`].
pub fn reconcile_bracket(
    root: &Path,
    owner: &Owner,
    booking: &Booking,
    direction: ReconcileDirection,
    modified: &[PathBuf],
) {
    reconcile_bracket_in(&locks_dir(), root, owner, booking, direction, modified);
}

/// Transfers a merged agent worktree's UNPAID debt into the owning root's
/// ledger — the merge bracket's only booking leg (wf-01; the misc-189
/// intersection, relocated from the retired land engine).
///
/// Debt means "an agent edited this and nobody has looked," not "content
/// moved" (the pull-parity ruling): the transfer set is the intersection of
/// the WORKTREE root's still-due ledger entries — each mapped worktree→owning
/// by its root-relative path — with `merged`, the canonical owning-root paths
/// the merge actually changed. A paid worktree (empty ledger) transfers
/// nothing; an unpaid file that did NOT merge (e.g. uncommitted worktree work
/// a squash merge never carries) transfers nothing. The transfer books under
/// `owner` — the identity that ran the merge — via [`book_transferred_in`], so
/// the lead's next bare `catenary diagnostics` serves exactly the inherited
/// set.
///
/// Spelling rule (misc 193): both roots canonicalize at this seam and each
/// mapped path re-canonicalizes, so the mapping keys the SAME canonical
/// ledgers the edit seams booked under regardless of any symlink alias the
/// caller resolved through. `merged` must already carry canonical spellings
/// (the caller canonicalizes at the git-oracle seam). Pin:
/// [`merge_transfer_books_through_aliased_spelling_into_canonical_ledger`].
///
/// The worktree's own ledger is deliberately left untouched: it retires with
/// the worktree (the disposal machinery removes the lock dir), and a KEPT
/// worktree's worker still owes its own gate — least surprising both ways.
/// Best-effort throughout: a booking failure never blocks the merge (it
/// already ran).
pub fn merge_transfer_in(
    locks_base: &Path,
    worktree_root: &Path,
    owning_root: &Path,
    owner: &Owner,
    merged: &std::collections::BTreeSet<PathBuf>,
) {
    let worktree_root = worktree_root
        .canonicalize()
        .unwrap_or_else(|_| worktree_root.to_path_buf());
    let owning_root = owning_root
        .canonicalize()
        .unwrap_or_else(|_| owning_root.to_path_buf());
    let mut transfer: Vec<PathBuf> = Vec::new();
    for unpaid in due_files_in(locks_base, &worktree_root) {
        let Ok(rel) = unpaid.strip_prefix(&worktree_root) else {
            continue; // not under the worktree — never part of this merge's debt
        };
        let mapped = owning_root.join(rel);
        let mapped = mapped.canonicalize().unwrap_or(mapped);
        if merged.contains(&mapped) {
            transfer.push(mapped);
        }
    }
    if !transfer.is_empty() {
        book_transferred_in(locks_base, &owning_root, owner, &transfer);
    }
}

/// Production wrapper for [`merge_transfer_in`] resolving the base through
/// [`locks_dir`].
pub fn merge_transfer(
    worktree_root: &Path,
    owning_root: &Path,
    owner: &Owner,
    merged: &std::collections::BTreeSet<PathBuf>,
) {
    merge_transfer_in(&locks_dir(), worktree_root, owning_root, owner, merged);
}

/// Unlinks a set of delivered files from their ledgers, grouping each file by its
/// own resolved lock root ([`resolve_lock_root`]) so a set spanning multiple
/// kitchens pays each (root-ownership stage 3).
///
/// The whole-set convenience over [`unlink_delivered`], used where the caller
/// holds a flat file list rather than a per-root grouping (the diagnostics
/// delivery seam). Each file is canonicalized
/// inside [`unlink_delivered_in`] so a symlinked-prefix alias pays the canonical
/// ledger it booked against (misc 193). Best-effort: a file that resolves to no
/// root unlinks nothing and reports [`UnlinkOutcome::NoRoot`]. Returns every
/// entry's [`UnlinkOutcome`] so the delivery seam can trace the payment
/// per file (bug 120); callers with no forensic seam ignore the receipt.
#[allow(
    clippy::must_use_candidate,
    reason = "called for the unlink side effect; the outcome receipt is optional forensics (bug 120)"
)]
pub fn unlink_delivered_by_root(files: &[PathBuf]) -> Vec<(PathBuf, UnlinkOutcome)> {
    use std::collections::HashMap;
    let mut by_root: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    let mut outcomes = Vec::with_capacity(files.len());
    for file in files {
        if let Some(root) = resolve_lock_root(file) {
            by_root.entry(root).or_default().push(file.clone());
        } else {
            outcomes.push((file.clone(), UnlinkOutcome::NoRoot));
        }
    }
    for (root, served) in &by_root {
        outcomes.extend(unlink_delivered(root, served));
    }
    outcomes
}

/// Retires a root's lock entirely under `locks_base`: removes
/// `<locks_base>/<encoded-root>/` (owner file, ledger, and all) — release leg 2
/// (root retirement).
///
/// Called when a worktree is removed / vanishes: the lock and its ledger
/// go with the kitchen. Idempotent — retiring an already-absent lock is a no-op.
/// Best-effort; a partial failure leaves the reaper to finish.
pub fn retire_in(locks_base: &Path, root: &Path) {
    let lock_dir = root_lock_dir_in(locks_base, root);
    let _ = std::fs::remove_dir_all(&lock_dir);
}

/// Production wrapper for [`retire_in`] resolving the base through [`locks_dir`].
pub fn retire(root: &Path) {
    retire_in(&locks_dir(), root);
}

/// The daemon's paid-idle countdown sweep under `locks_base` (release leg 1).
///
/// Scans `locks_base` and removes every lock directory that is **paid** (empty
/// `dir/`) and whose owner's last activity is older than `idle_timeout`. A lock
/// with due files, or one touched within the window, survives untouched — a
/// re-edit inside the window re-arms it with no new ceremony. Returns the encoded
/// names of the reaped lock dirs (for logging).
///
/// This is the ONLY timer-driven release. It is indifferent to daemon lifecycle:
/// the daemon owns the countdown, but a lock survives daemon churn and reboots by
/// construction (nothing here keys on the daemon's identity or uptime).
#[must_use]
pub fn reap_paid_idle_locks_in(
    locks_base: &Path,
    now: SystemTime,
    idle_timeout: Duration,
) -> Vec<String> {
    let mut reaped = Vec::new();
    let Ok(entries) = std::fs::read_dir(locks_base) else {
        return reaped;
    };
    for entry in entries.flatten() {
        let lock_dir = entry.path();
        if !lock_dir.is_dir() {
            continue;
        }
        // Paid ⇒ empty ledger.
        if due_count(&lock_dir) != 0 {
            continue;
        }
        // Idle past the window? An ownerless dir (a crashed acquisition) ages by
        // its own dir mtime — `last_activity_age`'s fallback — so it is cleared
        // here like any paid idle lock instead of holding the root hostage. Only
        // a genuinely unreadable / future mtime is "unknown, not yet expired" —
        // left for the next sweep to re-check.
        let expired = last_activity_age(&lock_dir, now).is_some_and(|age| age >= idle_timeout);
        if expired
            && std::fs::remove_dir_all(&lock_dir).is_ok()
            && let Some(name) = lock_dir.file_name().and_then(|n| n.to_str())
        {
            reaped.push(name.to_string());
        }
    }
    reaped
}

/// Production wrapper for [`reap_paid_idle_locks_in`] resolving the base through
/// [`locks_dir`].
#[must_use]
pub fn reap_paid_idle_locks(now: SystemTime, idle_timeout: Duration) -> Vec<String> {
    reap_paid_idle_locks_in(&locks_dir(), now, idle_timeout)
}

/// Static, daemon-free booking data: which file types map to a
/// configured-or-blessed diagnostic feeder (LSP server or linter).
///
/// Built once per hook process from the merged config ([`crate::config::Config`]
/// — defaults ∪ user), so the edit seam decides "does this file book" without
/// any daemon round-trip. This is a static approximation of the daemon's
/// [`covered_for_diagnostics`](crate::bridge::session::Session::covered_for_diagnostics)
/// gate: it agrees on the dominant population (a file whose language has a
/// configured server, or which a linter rule routes to). Over-booking a
/// configured-but-dead server is safe — it renders honestly at serve time and
/// retires — so the static view intentionally errs toward covering.
///
/// Convergence note (lsm 05): this static view also over-books files whose
/// server is configured-but-not-installed (the language books, but no binary
/// can ever serve the debt — it renders and retires as above). `[servers]
/// auto_install` narrows that gap toward zero: a blessed server a root's
/// markers want is background-installed at session start, so by serve time the
/// booking is usually honest coverage rather than a benign over-book. No
/// booking-side change is needed — the gap closes from the install side.
pub struct Booking {
    /// File extension (no dot) → language books (has a configured server).
    extensions: std::collections::HashMap<String, bool>,
    /// Exact filename → language books.
    filenames: std::collections::HashMap<String, bool>,
    /// Linter path-glob matchers — a file whose root-relative path matches any
    /// of these books even without a language-server binding.
    linters: Vec<crate::config::LinterConfig>,
}

impl Booking {
    /// Builds the booking tables from a merged config.
    ///
    /// A language books iff its `servers` list is non-empty (at least one
    /// configured `[lsp.server.*]` binding). Extensions and exact filenames
    /// inherit their language's booking status. Linter rules with compiled
    /// patterns are carried whole for the path-glob check in [`Self::books`].
    #[must_use]
    pub fn from_config(config: &crate::config::Config) -> Self {
        let mut extensions = std::collections::HashMap::new();
        let mut filenames = std::collections::HashMap::new();

        for lc in config.language.values() {
            let books = !lc.servers().is_empty();
            if let Some(ref exts) = lc.extensions {
                for ext in exts {
                    // A booking language upgrades a non-booking prior claim on
                    // the same extension (`or` accumulation).
                    let entry = extensions.entry(ext.clone()).or_insert(false);
                    *entry = *entry || books;
                }
            }
            if let Some(ref fnames) = lc.filenames {
                for fname in fnames {
                    let entry = filenames.entry(fname.clone()).or_insert(false);
                    *entry = *entry || books;
                }
            }
        }

        let linters = config.linter.values().cloned().collect();

        Self {
            extensions,
            filenames,
            linters,
        }
    }

    /// Whether an edited `file` books — its type maps to a configured LSP server
    /// or a linter rule routes to it.
    ///
    /// Checks exact filename first, then extension (mirroring the daemon's
    /// classification precedence, minus the shebang slow path — a bookable
    /// extensionless script is rare and over/under-booking it is safe this
    /// stage). Then the linter path-globs, matched against the file's
    /// root-relative path when a lock root resolves (else the file name).
    #[must_use]
    pub fn books(&self, file: &Path) -> bool {
        if let Some(name) = file.file_name().and_then(|n| n.to_str())
            && self.filenames.get(name).copied().unwrap_or(false)
        {
            return true;
        }
        if let Some(ext) = file.extension().and_then(|e| e.to_str())
            && self.extensions.get(ext).copied().unwrap_or(false)
        {
            return true;
        }
        // Linter routing by root-relative path glob. Resolve the file's root so
        // an anchored pattern (`.github/workflows/*.yml`) matches; fall back to
        // the file name for an unrooted file.
        let rel = resolve_lock_root(file)
            .and_then(|root| {
                file.strip_prefix(&root)
                    .ok()
                    .map(std::path::Path::to_path_buf)
            })
            .unwrap_or_else(|| PathBuf::from(file.file_name().unwrap_or(file.as_os_str())));
        self.linters.iter().any(|l| l.matches(&rel))
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect/panic for readable assertions"
)]
mod tests {
    use super::*;

    /// A tempdir carrying a repo root (`.git` marker) and a `locks/` base, so the
    /// lock ops stay under the tempdir without touching the process environment.
    struct Fixture {
        dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let root = dir.path().join("repo");
            std::fs::create_dir_all(root.join(".git")).expect("mk .git");
            std::fs::create_dir_all(root.join("src")).expect("mk src");
            let root = root.canonicalize().expect("canon root");
            Self { dir, root }
        }

        fn locks(&self) -> PathBuf {
            self.dir.path().join("locks")
        }

        fn file(&self, rel: &str) -> PathBuf {
            let p = self.root.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).expect("mk parent");
            }
            std::fs::write(&p, b"").expect("write file");
            p
        }
    }

    fn rust_booking() -> Booking {
        let config = crate::config::Config::load().expect("load config");
        Booking::from_config(&config)
    }

    #[test]
    fn owner_file_name_round_trips() {
        let o = Owner::new("claude", "sess-abc", "agent-1");
        assert_eq!(o.file_name(), "claude+sess-abc+agent-1");
        assert_eq!(Owner::parse("claude+sess-abc+agent-1"), Some(o));
    }

    #[test]
    fn owner_parse_rejects_non_owner_names() {
        assert!(Owner::parse("dir").is_none());
        assert!(Owner::parse("noplus").is_none());
        assert!(Owner::parse("only+one").is_none());
    }

    #[test]
    fn empty_agent_is_main_agent() {
        let o = Owner::new("claude", "sess-abc", "");
        assert_eq!(o.file_name(), "claude+sess-abc+");
        assert_eq!(Owner::parse("claude+sess-abc+"), Some(o));
    }

    #[test]
    fn booking_books_rust_not_txt() {
        let b = rust_booking();
        assert!(b.books(Path::new("/x/main.rs")), "rust books (has server)");
        assert!(
            !b.books(Path::new("/x/notes.txt")),
            "txt does not book (no server)"
        );
    }

    #[test]
    fn first_cook_claims_and_books() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let got = acquire_in(
            &fx.locks(),
            &file,
            &owner,
            &rust_booking(),
            SystemTime::now(),
        );
        assert!(matches!(got, Acquired::Ours), "first cook admitted");

        let lock_dir = root_lock_dir_in(&fx.locks(), &fx.root);
        assert!(lock_dir.is_dir(), "lock dir created");
        assert_eq!(
            read_owner_name(&lock_dir).expect("read owner"),
            Some(owner.file_name()),
            "owner file titled with the tuple"
        );
        assert_eq!(due_count(&lock_dir), 1, "the edited file is booked");
    }

    #[test]
    fn second_cook_denied_with_briefing() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let a = Owner::new("claude", "sess-a", "");
        let b = Owner::new("claude", "sess-b", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        assert!(matches!(
            acquire_in(&locks, &file, &a, &booking, now),
            Acquired::Ours
        ));
        let denied = acquire_in(&locks, &file, &b, &booking, now);
        let Acquired::Denied(msg) = denied else {
            panic!("second cook must be denied");
        };
        assert!(msg.contains("root locked:"), "briefing header: {msg}");
        assert!(msg.contains("catenary claim"), "names claim: {msg}");
        assert!(
            msg.contains("1 file awaiting diagnosis"),
            "reports the due count: {msg}"
        );
        // First cook unaffected — a re-edit is still admitted.
        assert!(matches!(
            acquire_in(&locks, &file, &a, &booking, now),
            Acquired::Ours
        ));
    }

    #[test]
    fn re_edit_is_idempotent_book() {
        let fx = Fixture::new();
        let f1 = fx.file("src/a.rs");
        let f2 = fx.file("src/b.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &f1, &owner, &booking, now),
            Acquired::Ours
        ));
        assert!(matches!(
            acquire_in(&locks, &f2, &owner, &booking, now),
            Acquired::Ours
        ));
        // Re-edit f1 — no double count.
        assert!(matches!(
            acquire_in(&locks, &f1, &owner, &booking, now),
            Acquired::Ours
        ));
        let lock_dir = root_lock_dir_in(&locks, &fx.root);
        assert_eq!(
            due_count(&lock_dir),
            2,
            "two distinct files booked once each"
        );
    }

    #[test]
    fn empty_dir_reading_denies() {
        let fx = Fixture::new();
        // Simulate a mid-swing acquisition: the lock dir exists but no owner
        // file is present yet. A FRESH ownerless dir (inside the grace) keeps
        // the ordinary mid-swing deny — the race semantics stand.
        let lock_dir = root_lock_dir_in(&fx.locks(), &fx.root);
        std::fs::create_dir_all(&lock_dir).expect("mk lock dir");
        let file = fx.file("src/main.rs");
        let b = Owner::new("claude", "sess-b", "");
        let got = acquire_in(&fx.locks(), &file, &b, &rust_booking(), SystemTime::now());
        let Acquired::Denied(msg) = got else {
            panic!("an empty (owner-less) lock dir denies the entrant");
        };
        assert!(
            msg.contains("held by another agent"),
            "inside the grace the deny is the ordinary mid-swing briefing, got: {msg}"
        );
        assert!(
            !msg.contains("interrupted"),
            "a fresh ownerless dir must not read as a crashed acquisition, got: {msg}"
        );
    }

    #[test]
    fn aged_ownerless_dir_denies_with_interrupted_briefing() {
        let fx = Fixture::new();
        // A crashed acquisition: the dir exists, no owner file, and it has aged
        // past the mid-swing grace (`now` is injected, so no wall-clock wait —
        // the dir's real mtime is "now", and the acquire runs 60s later).
        let lock_dir = root_lock_dir_in(&fx.locks(), &fx.root);
        std::fs::create_dir_all(&lock_dir).expect("mk lock dir");
        let file = fx.file("src/main.rs");
        let b = Owner::new("claude", "sess-b", "");
        let later = SystemTime::now() + Duration::from_mins(1);
        let got = acquire_in(&fx.locks(), &file, &b, &rust_booking(), later);
        let Acquired::Denied(msg) = got else {
            panic!("an aged ownerless dir still denies (claim is the rescue)");
        };
        assert!(
            msg.contains("interrupted lock acquisition"),
            "past the grace the deny tells the truth, got: {msg}"
        );
        assert!(
            msg.contains(&format!("catenary claim {}", fx.root.display())),
            "the interrupted briefing points at the claim rescue, got: {msg}"
        );
        assert!(
            !msg.contains("held by another agent"),
            "no phantom agent is claimed for a crashed acquisition, got: {msg}"
        );
    }

    #[test]
    fn reaper_clears_aged_ownerless_dir() {
        let fx = Fixture::new();
        // A crashed acquisition left an ownerless dir. `due_count == 0` (no
        // ledger) and the dir's own mtime is the age clock, so the ordinary
        // paid-idle sweep clears it — never a permanent hostage.
        let lock_dir = root_lock_dir_in(&fx.locks(), &fx.root);
        std::fs::create_dir_all(&lock_dir).expect("mk lock dir");

        // Inside the window it survives (a genuinely mid-swing dir must not be
        // swept out from under its acquirer).
        let now = SystemTime::now();
        let reaped = reap_paid_idle_locks_in(&fx.locks(), now, PAID_IDLE_TIMEOUT);
        assert!(
            reaped.is_empty(),
            "a fresh ownerless dir survives the sweep"
        );
        assert!(lock_dir.is_dir());

        // Past the window it is reaped by dir mtime.
        let future = now + Duration::from_secs(10_000);
        let reaped = reap_paid_idle_locks_in(&fx.locks(), future, PAID_IDLE_TIMEOUT);
        assert_eq!(reaped.len(), 1, "an aged ownerless dir is reaped");
        assert!(!lock_dir.exists(), "the crashed-acquisition dir is removed");
    }

    #[test]
    fn claim_rescues_ownerless_dir() {
        let fx = Fixture::new();
        // The explicit rescue: claiming a dir-exists-ownerless lock WRITES the
        // claimant's owner file instead of failing, so a human/agent does not
        // wait for the reaper.
        let lock_dir = root_lock_dir_in(&fx.locks(), &fx.root);
        std::fs::create_dir_all(&lock_dir).expect("mk lock dir");
        let b = Owner::new("claude", "sess-b", "");
        let now = SystemTime::now();

        let claimed = claim_in(&fx.locks(), &fx.root, &b, now);
        let Claimed::Ok { due, .. } = claimed else {
            panic!("claim must rescue an ownerless dir, got {claimed:?}");
        };
        assert_eq!(due, 0, "a crashed acquisition booked nothing");
        assert_eq!(
            read_owner_name(&lock_dir).expect("read owner"),
            Some(b.file_name()),
            "the claimant's owner file is installed"
        );

        // The rescue is complete: the claimant edits freely, others are denied.
        let file = fx.file("src/main.rs");
        assert!(matches!(
            acquire_in(&fx.locks(), &file, &b, &rust_booking(), now),
            Acquired::Ours
        ));
        let a = Owner::new("claude", "sess-a", "");
        assert!(matches!(
            acquire_in(&fx.locks(), &file, &a, &rust_booking(), now),
            Acquired::Denied(_)
        ));
    }

    #[test]
    fn uncovered_standalone_edit_takes_no_lock() {
        let fx = Fixture::new();
        let file = fx.file("notes.txt");
        let owner = Owner::new("claude", "sess-a", "");
        let got = acquire_in(
            &fx.locks(),
            &file,
            &owner,
            &rust_booking(),
            SystemTime::now(),
        );
        assert!(matches!(got, Acquired::Ours), "uncovered edit admitted");
        assert!(
            !root_lock_dir_in(&fx.locks(), &fx.root).exists(),
            "an uncovered standalone edit takes no lock (booking honesty)"
        );
    }

    #[test]
    fn foreign_territory_takes_no_lock() {
        let fx = Fixture::new();
        // A path with no repository marker above it.
        let scratch = tempfile::tempdir().expect("scratch");
        let file = scratch.path().join("scratch.rs");
        std::fs::write(&file, "fn main() {}").expect("write");
        let owner = Owner::new("claude", "sess-a", "");
        let got = acquire_in(
            &fx.locks(),
            &file,
            &owner,
            &rust_booking(),
            SystemTime::now(),
        );
        assert!(matches!(got, Acquired::Ours), "foreign edit admitted");
        assert!(
            resolve_lock_root(&file).is_none(),
            "scratch path resolves to no lock root"
        );
    }

    #[test]
    fn delivery_unlinks_and_starts_countdown() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        assert!(matches!(
            acquire_in(
                &fx.locks(),
                &file,
                &owner,
                &rust_booking(),
                SystemTime::now()
            ),
            Acquired::Ours
        ));
        let lock_dir = root_lock_dir_in(&fx.locks(), &fx.root);
        assert_eq!(due_count(&lock_dir), 1);

        unlink_delivered_in(&fx.locks(), &fx.root, std::slice::from_ref(&file));
        assert_eq!(due_count(&lock_dir), 0, "delivery unlinks the touch file");
        // Payment is parole — the lock survives.
        assert!(lock_dir.is_dir(), "lock is NOT removed at payment");
    }

    /// The delivery unlink's forensic receipt (bug 120): each entry names where
    /// the payment went — `NoLedger` for an unlocked root, `Unlinked` for a
    /// paid leaf, `NoEntry` for a serve with no standing debt.
    #[test]
    fn unlink_outcomes_name_where_payment_went() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let locks = fx.locks();

        // No lock dir yet — nothing to pay.
        let got = unlink_delivered_in(&locks, &fx.root, std::slice::from_ref(&file));
        assert_eq!(got, vec![(file.clone(), UnlinkOutcome::NoLedger)]);

        // Booked, then delivered — the leaf pays.
        assert!(matches!(
            acquire_in(&locks, &file, &owner, &rust_booking(), SystemTime::now()),
            Acquired::Ours
        ));
        let got = unlink_delivered_in(&locks, &fx.root, std::slice::from_ref(&file));
        assert_eq!(got, vec![(file.clone(), UnlinkOutcome::Unlinked)]);

        // Delivered again with the lock dir surviving (payment is parole): no
        // leaf remains — the skip is reported, not silent.
        let got = unlink_delivered_in(&locks, &fx.root, std::slice::from_ref(&file));
        assert_eq!(got, vec![(file, UnlinkOutcome::NoEntry)]);
    }

    #[test]
    fn re_edit_inside_window_re_arms_same_lock() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &file, &owner, &booking, now),
            Acquired::Ours
        ));
        unlink_delivered_in(&locks, &fx.root, std::slice::from_ref(&file));
        let lock_dir = root_lock_dir_in(&locks, &fx.root);
        assert_eq!(due_count(&lock_dir), 0, "paid");
        // A re-edit inside the idle window re-books under the same owner.
        assert!(matches!(
            acquire_in(&locks, &file, &owner, &booking, now),
            Acquired::Ours
        ));
        assert_eq!(due_count(&lock_dir), 1, "re-book re-arms the lock");
        assert_eq!(
            read_owner_name(&lock_dir).expect("read owner"),
            Some(owner.file_name()),
            "same owner, no new ceremony"
        );
    }

    #[test]
    fn reaper_removes_paid_idle_lock_only() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &file, &owner, &rust_booking(), now),
            Acquired::Ours
        ));
        let lock_dir = root_lock_dir_in(&locks, &fx.root);

        // Unpaid lock is never reaped, even when idle.
        let future = now + Duration::from_secs(10_000);
        let reaped = reap_paid_idle_locks_in(&locks, future, PAID_IDLE_TIMEOUT);
        assert!(reaped.is_empty(), "an unpaid lock is never reaped");
        assert!(lock_dir.is_dir());

        // Pay it, then reap when idle past the window.
        unlink_delivered_in(&locks, &fx.root, std::slice::from_ref(&file));
        let reaped = reap_paid_idle_locks_in(&locks, future, PAID_IDLE_TIMEOUT);
        assert_eq!(reaped.len(), 1, "paid + idle → reaped");
        assert!(!lock_dir.exists(), "the paid idle lock dir is removed");
    }

    #[test]
    fn paid_lock_inside_window_survives_reap() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &file, &owner, &rust_booking(), now),
            Acquired::Ours
        ));
        unlink_delivered_in(&locks, &fx.root, std::slice::from_ref(&file));
        // Reap "now" — inside the window, so it survives.
        let reaped = reap_paid_idle_locks_in(&locks, now, PAID_IDLE_TIMEOUT);
        assert!(reaped.is_empty(), "a paid lock inside its window survives");
        assert!(root_lock_dir_in(&locks, &fx.root).is_dir());
    }

    #[test]
    fn retire_removes_lock_and_ledger() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        assert!(matches!(
            acquire_in(
                &fx.locks(),
                &file,
                &owner,
                &rust_booking(),
                SystemTime::now()
            ),
            Acquired::Ours
        ));
        let lock_dir = root_lock_dir_in(&fx.locks(), &fx.root);
        assert!(lock_dir.is_dir());
        retire_in(&fx.locks(), &fx.root);
        assert!(!lock_dir.exists(), "retirement removes the whole lock dir");
        // Idempotent.
        retire_in(&fx.locks(), &fx.root);
    }

    #[test]
    fn claim_transfers_owner_and_inherits_debt() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let a = Owner::new("claude", "sess-a", "");
        let b = Owner::new("claude", "sess-b", "");
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &file, &a, &rust_booking(), now),
            Acquired::Ours
        ));
        let lock_dir = root_lock_dir_in(&locks, &fx.root);
        assert_eq!(due_count(&lock_dir), 1, "a's debt");

        let claimed = claim_in(&locks, &fx.root, &b, now);
        let Claimed::Ok { due, .. } = claimed else {
            panic!("claim must succeed, got {claimed:?}");
        };
        assert_eq!(due, 1, "the claimant inherits a's due set");
        assert_eq!(
            read_owner_name(&lock_dir).expect("read owner"),
            Some(b.file_name()),
            "owner file re-titled to the claimant"
        );
        // The inherited ledger survives the rename.
        assert_eq!(due_count(&lock_dir), 1, "debt rides the takeover");
        // b now edits freely; a is denied.
        assert!(matches!(
            acquire_in(&locks, &file, &b, &rust_booking(), now),
            Acquired::Ours
        ));
        assert!(matches!(
            acquire_in(&locks, &file, &a, &rust_booking(), now),
            Acquired::Denied(_)
        ));
    }

    #[test]
    fn claim_unlocked_root_is_noop() {
        let fx = Fixture::new();
        let b = Owner::new("claude", "sess-b", "");
        let claimed = claim_in(&fx.locks(), &fx.root, &b, SystemTime::now());
        assert!(matches!(claimed, Claimed::Unlocked), "nothing to claim");
    }

    #[test]
    fn claim_own_lock_is_already_ours() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let a = Owner::new("claude", "sess-a", "");
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &file, &a, &rust_booking(), now),
            Acquired::Ours
        ));
        let claimed = claim_in(&locks, &fx.root, &a, now);
        assert!(matches!(claimed, Claimed::AlreadyOurs));
    }

    #[test]
    fn deny_briefing_shape_matches_ruled_copy() {
        let root = Path::new("/home/mark/Projects/Catenary");
        let msg = deny_briefing(root, Some(Duration::from_secs(43)), 3, false);
        assert!(msg.starts_with("root locked: /home/mark/Projects/Catenary\n"));
        assert!(msg.contains("last activity 43s ago; 3 files awaiting diagnosis."));
        assert!(msg.contains("  catenary claim /home/mark/Projects/Catenary"));
        assert!(msg.contains("Claiming transfers the root and its diagnostic debt to you."));
    }

    #[test]
    fn deny_briefing_softens_when_paid_and_idle() {
        let root = Path::new("/home/mark/Projects/Catenary");
        let msg = deny_briefing(root, Some(Duration::from_mins(2)), 0, true);
        assert!(msg.contains("no unpaid debt; lock expires shortly."));
    }

    #[test]
    fn claim_answer_shape_matches_ruled_copy() {
        let root = Path::new("/home/mark/Projects/Catenary");
        let msg = claim_answer(root, Some(Duration::from_mins(26)), 3, false);
        assert!(msg.starts_with(
            "claimed /home/mark/Projects/Catenary (previous editor last seen 26m ago)\n"
        ));
        assert!(
            msg.contains("3 files awaiting diagnosis — run `catenary diagnostics` to serve them.")
        );
        assert!(msg.contains("Review the inherited edits with git diff / git status."));
    }

    #[test]
    fn claim_answer_softens_when_paid_and_idle() {
        let root = Path::new("/home/mark/Projects/Catenary");
        let msg = claim_answer(root, Some(Duration::from_mins(2)), 0, true);
        assert!(msg.contains("no unpaid debt; lock expires shortly."));
    }

    // ── Ledger as source of truth (root-ownership stage 3) ──────────────

    #[test]
    fn due_files_reconstructs_absolute_paths_from_the_ledger() {
        let fx = Fixture::new();
        let f1 = fx.file("src/main.rs");
        let f2 = fx.file("src/inner/lib.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &f1, &owner, &booking, now),
            Acquired::Ours
        ));
        assert!(matches!(
            acquire_in(&locks, &f2, &owner, &booking, now),
            Acquired::Ours
        ));

        // The due set read from disk IS the batch — the absolute paths the edit
        // seam booked, reconstructed from the `.lock` touch-tree.
        let due = due_files_in(&locks, &fx.root);
        assert_eq!(
            due,
            vec![f2.clone(), f1.clone()]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>(),
            "due_files rebuilds both booked paths (sorted)"
        );
        assert!(has_debt_in(&locks, &fx.root), "unpaid debt is present");

        // Delivery unlinks — the ledger empties, so the due set drains and the
        // debt clears (the new bare-rerun contract: no debt after full payment).
        unlink_delivered_in(&locks, &fx.root, &[f1, f2]);
        assert!(
            due_files_in(&locks, &fx.root).is_empty(),
            "a fully-paid ledger reports no due files"
        );
        assert!(
            !has_debt_in(&locks, &fx.root),
            "a fully-paid ledger reports no debt"
        );
    }

    #[test]
    fn due_files_empty_for_unlocked_root() {
        let fx = Fixture::new();
        assert!(
            due_files_in(&fx.locks(), &fx.root).is_empty(),
            "a root with no lock has no due files"
        );
        assert!(
            !has_debt_in(&fx.locks(), &fx.root),
            "a root with no lock has no debt"
        );
    }

    #[test]
    fn claim_marker_is_written_and_taken_once() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let a = Owner::new("claude", "sess-a", "");
        let b = Owner::new("claude", "sess-b", "");
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &file, &a, &rust_booking(), now),
            Acquired::Ours
        ));
        // Before any claim there is no breadcrumb.
        assert!(
            !take_claim_marker_in(&locks, &fx.root),
            "no marker before a claim"
        );

        // A claim drops the breadcrumb.
        let claimed = claim_in(&locks, &fx.root, &b, now);
        assert!(matches!(claimed, Claimed::Ok { .. }), "claim succeeds");

        // The first serve reads-and-removes it (one-shot).
        assert!(
            take_claim_marker_in(&locks, &fx.root),
            "the first serve after a claim sees the breadcrumb"
        );
        assert!(
            !take_claim_marker_in(&locks, &fx.root),
            "the breadcrumb is one-shot — a later serve sees nothing"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_spelling_books_into_the_same_ledger() {
        // The macOS ingestion-seam rule (misc 193): an edit reached through a
        // symlinked-prefix alias of the root must book into — and pay off — the
        // SAME canonical ledger the direct spelling uses. Without canonicalizing
        // at the seam the two spellings would split the debt.
        let fx = Fixture::new();
        // A symlink whose target is the canonical repo root.
        let alias = fx.dir.path().join("alias");
        std::os::unix::fs::symlink(&fx.root, &alias).expect("mk symlink");
        // The file reached through the alias spelling.
        let real = fx.file("src/main.rs");
        let via_alias = alias.join("src/main.rs");

        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        // Book through the alias spelling.
        assert!(matches!(
            acquire_in(&locks, &via_alias, &owner, &booking, now),
            Acquired::Ours
        ));
        // The debt landed on the CANONICAL ledger, and the reconstructed due path
        // is the canonical spelling.
        assert_eq!(
            due_files_in(&locks, &fx.root),
            vec![real],
            "the aliased edit booked into the canonical ledger"
        );

        // Paying through the alias spelling clears the canonical ledger.
        unlink_delivered_in(&locks, &fx.root, std::slice::from_ref(&via_alias));
        assert!(
            due_files_in(&locks, &fx.root).is_empty(),
            "paying through the alias spelling clears the canonical debt"
        );
    }

    // ── The Stop-block candidate check (bug 116) ───────────────────────

    #[test]
    fn due_candidates_returns_only_the_unpaid() {
        // The Stop-block core: given the edited candidate set, return exactly the
        // files still due on their root's ledger. A booked file is due; an unbooked
        // one is not.
        let fx = Fixture::new();
        let a = fx.file("src/a.rs");
        let b = fx.file("src/b.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        // Only `a` is booked (edited); `b` was never covered.
        assert!(matches!(
            acquire_in(&locks, &a, &owner, &booking, now),
            Acquired::Ours
        ));
        let due = due_candidates_in(&locks, &[a.clone(), b.clone()]);
        assert_eq!(due, vec![a.clone()], "only the booked candidate is due");

        // Paying `a` (delivery unlinks its touch file) empties the due set.
        unlink_delivered_in(&locks, &fx.root, std::slice::from_ref(&a));
        assert!(
            due_candidates_in(&locks, &[a, b]).is_empty(),
            "a paid candidate is no longer due"
        );
    }

    #[test]
    fn due_candidates_skips_foreign_territory() {
        // A candidate outside any VCS checkout resolves to no root — never booked,
        // never due, so it is silently dropped from the due set (no false block).
        let fx = Fixture::new();
        let scratch = fx.dir.path().join("scratch.rs");
        std::fs::write(&scratch, b"").expect("write scratch");
        assert!(
            due_candidates_in(&fx.locks(), &[scratch]).is_empty(),
            "a foreign-territory candidate books no debt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn due_candidates_read_through_aliased_spelling() {
        // Path-spelling pin (bug 116 / misc 193): a candidate reached through a
        // symlinked-prefix alias of the root must resolve to the SAME canonical
        // ledger answer as the direct spelling — else the Stop-block would split
        // from the debt. Linux-green does not prove this; the symlink alias is the
        // regression pin.
        let fx = Fixture::new();
        let alias = fx.dir.path().join("alias");
        std::os::unix::fs::symlink(&fx.root, &alias).expect("mk symlink");
        let real = fx.file("src/main.rs");
        let via_alias = alias.join("src/main.rs");

        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        // Book against the canonical ledger.
        assert!(matches!(
            acquire_in(&locks, &real, &owner, &booking, now),
            Acquired::Ours
        ));

        // The candidate carried in the ALIASED spelling resolves to the canonical
        // due path — the aliased answer matches the direct answer.
        assert_eq!(
            due_candidates_in(&locks, std::slice::from_ref(&via_alias)),
            vec![real.clone()],
            "the aliased candidate resolves to the canonical due path"
        );

        // Paying the canonical ledger clears the aliased candidate too.
        unlink_delivered_in(&locks, &fx.root, std::slice::from_ref(&real));
        assert!(
            due_candidates_in(&locks, &[via_alias]).is_empty(),
            "the aliased candidate reads the same paid canonical ledger"
        );
    }

    #[test]
    fn book_transferred_books_into_the_ledger_under_an_owner() {
        // The bulk booking seam behind the merge bracket (wf-01): files become
        // the merging agent's debt in the owning repo's ledger, served by a
        // later `catenary diagnostics` there. Booking creates the lock dir +
        // owner file when the root is not yet locked.
        let fx = Fixture::new();
        let f1 = fx.file("src/a.rs");
        let f2 = fx.file("src/b.rs");
        let owner = Owner::new("claude", "sess-land", "");
        let locks = fx.locks();

        assert!(
            !root_lock_dir_in(&locks, &fx.root).exists(),
            "no lock before the transfer"
        );
        book_transferred_in(&locks, &fx.root, &owner, &[f1.clone(), f2.clone()]);

        let lock_dir = root_lock_dir_in(&locks, &fx.root);
        assert!(lock_dir.is_dir(), "the transfer creates the lock dir");
        assert_eq!(
            read_owner_name(&lock_dir).expect("read owner"),
            Some(owner.file_name()),
            "the landing agent owns the inherited debt"
        );
        assert_eq!(
            due_files_in(&locks, &fx.root),
            {
                let mut v = vec![f1, f2];
                v.sort();
                v
            },
            "both transferred files are due in the owning repo's ledger"
        );
    }

    #[test]
    fn book_transferred_empty_is_a_noop() {
        let fx = Fixture::new();
        let owner = Owner::new("claude", "sess-land", "");
        book_transferred_in(&fx.locks(), &fx.root, &owner, &[]);
        assert!(
            !root_lock_dir_in(&fx.locks(), &fx.root).exists(),
            "an empty transfer books nothing and creates no lock"
        );
    }

    #[test]
    fn claim_marker_does_not_count_as_due() {
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let a = Owner::new("claude", "sess-a", "");
        let b = Owner::new("claude", "sess-b", "");
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &file, &a, &rust_booking(), now),
            Acquired::Ours
        ));
        let lock_dir = root_lock_dir_in(&locks, &fx.root);
        assert_eq!(due_count(&lock_dir), 1, "one due file before the claim");

        let _ = claim_in(&locks, &fx.root, &b, now);
        // The `.claimed` marker lives beside `dir/`, never inside it — the due
        // count must not change.
        assert_eq!(
            due_count(&lock_dir),
            1,
            "the breadcrumb is never miscounted as a due file"
        );
        assert_eq!(
            read_owner_name(&lock_dir).expect("read owner"),
            Some(b.file_name()),
            "the marker does not confuse owner-file resolution"
        );
    }

    // ── The reconcile bracket (root-ownership stage 5) ──────────────────────

    #[test]
    fn unbook_direction_clears_files_git_reports_clean() {
        // The edit → book → `git stash` (unbook) leg: an edited-and-booked file
        // that git now reports CLEAN (its modification left the working tree) is
        // unbooked. A file still modified stays due.
        let fx = Fixture::new();
        let stashed = fx.file("src/stashed.rs");
        let kept = fx.file("src/kept.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        // Both files are edited and booked.
        assert!(matches!(
            acquire_in(&locks, &stashed, &owner, &booking, now),
            Acquired::Ours
        ));
        assert!(matches!(
            acquire_in(&locks, &kept, &owner, &booking, now),
            Acquired::Ours
        ));
        assert_eq!(due_files_in(&locks, &fx.root).len(), 2, "both booked");

        // git now reports only `kept` modified (a `git stash` of `stashed`).
        reconcile_bracket_in(
            &locks,
            &fx.root,
            &owner,
            &booking,
            ReconcileDirection::Unbook,
            std::slice::from_ref(&kept),
        );

        assert_eq!(
            due_files_in(&locks, &fx.root),
            vec![kept],
            "the stashed file is unbooked; the still-modified file stays due"
        );
    }

    #[test]
    fn unbook_all_clean_clears_the_whole_ledger() {
        // A `git stash` of everything: git reports nothing modified, so the whole
        // ledger unbooks and a bare diagnose would answer no-debt.
        let fx = Fixture::new();
        let f1 = fx.file("src/a.rs");
        let f2 = fx.file("src/b.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();
        assert!(matches!(
            acquire_in(&locks, &f1, &owner, &booking, now),
            Acquired::Ours
        ));
        assert!(matches!(
            acquire_in(&locks, &f2, &owner, &booking, now),
            Acquired::Ours
        ));

        reconcile_bracket_in(
            &locks,
            &fx.root,
            &owner,
            &booking,
            ReconcileDirection::Unbook,
            &[],
        );

        assert!(
            !has_debt_in(&locks, &fx.root),
            "stashing everything clears the debt"
        );
    }

    #[test]
    fn book_direction_books_covered_modified_files() {
        // The `git stash pop` (book) leg: every COVERED file git reports modified
        // is booked ("the pop should present as writes that restore it"). An
        // uncoverable file (no server) is never booked — the same static gate the
        // edit seam uses.
        let fx = Fixture::new();
        let covered = fx.file("src/restored.rs");
        let uncovered = fx.file("notes.txt");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let locks = fx.locks();

        // The root is not yet locked (a pop into a fresh/paid kitchen).
        assert!(!root_lock_dir_in(&locks, &fx.root).exists());

        reconcile_bracket_in(
            &locks,
            &fx.root,
            &owner,
            &booking,
            ReconcileDirection::Book,
            &[covered.clone(), uncovered],
        );

        // Only the covered file booked; the ledger + owner were created.
        assert_eq!(
            due_files_in(&locks, &fx.root),
            vec![covered],
            "the pop books the covered restored file, never the uncoverable one"
        );
        let lock_dir = root_lock_dir_in(&locks, &fx.root);
        assert_eq!(
            read_owner_name(&lock_dir).expect("read owner"),
            Some(owner.file_name()),
            "the pop creates the owner file when the root was unlocked"
        );
    }

    #[test]
    fn round_trip_edit_stash_pop_re_books() {
        // The end-to-end round trip: edit → booked; stash → unbooked (no debt);
        // pop → re-booked (debt restored, ready to serve).
        let fx = Fixture::new();
        let file = fx.file("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        // edit → booked
        assert!(matches!(
            acquire_in(&locks, &file, &owner, &booking, now),
            Acquired::Ours
        ));
        assert!(has_debt_in(&locks, &fx.root), "edit books the file");

        // stash → git reports clean → unbooked
        reconcile_bracket_in(
            &locks,
            &fx.root,
            &owner,
            &booking,
            ReconcileDirection::Unbook,
            &[],
        );
        assert!(
            !has_debt_in(&locks, &fx.root),
            "the stash unbooks: bare diagnostics answers no-debt"
        );

        // pop → git reports it modified again → re-booked
        reconcile_bracket_in(
            &locks,
            &fx.root,
            &owner,
            &booking,
            ReconcileDirection::Book,
            std::slice::from_ref(&file),
        );
        assert_eq!(
            due_files_in(&locks, &fx.root),
            vec![file],
            "the pop re-books the restored file — diagnose serves it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_bracket_books_through_aliased_spelling_into_canonical_ledger() {
        // The spelling rule at the reconcile seam (misc 193): a `git status`
        // path reached through a symlinked-prefix alias of the root must book
        // into the SAME canonical ledger the edit seam pays against. A green
        // real-dir run masks this — the symlink alias is the regression pin.
        let fx = Fixture::new();
        let alias = fx.dir.path().join("alias");
        std::os::unix::fs::symlink(&fx.root, &alias).expect("mk symlink");
        // The real file, and its aliased spelling (what a status-in-the-alias
        // query would join).
        let real = fx.file("src/main.rs");
        let via_alias = alias.join("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let locks = fx.locks();

        // Book via the alias spelling (the pop direction).
        reconcile_bracket_in(
            &locks,
            &fx.root,
            &owner,
            &booking,
            ReconcileDirection::Book,
            std::slice::from_ref(&via_alias),
        );
        assert_eq!(
            due_files_in(&locks, &fx.root),
            vec![real.clone()],
            "the aliased pop books into the canonical ledger"
        );

        // Unbook via the alias spelling too (the stash direction): a status that
        // reports the aliased file still modified must NOT unbook the canonical
        // entry. Passing the alias spelling as the modified set keeps it due.
        reconcile_bracket_in(
            &locks,
            &fx.root,
            &owner,
            &booking,
            ReconcileDirection::Unbook,
            std::slice::from_ref(&via_alias),
        );
        assert_eq!(
            due_files_in(&locks, &fx.root),
            vec![real],
            "the aliased still-modified file is NOT wrongly unbooked (same canonical spelling)"
        );
    }

    // ── The merge bracket: unpaid-debt transfer only (wf-01) ────────────────

    /// A two-kitchen fixture for the merge transfer: an `owning/` repo root and
    /// a sibling `wt/` agent-worktree root (a `.git` FILE marker, as a real
    /// linked worktree carries), both canonical, sharing one locks base.
    struct MergeFixture {
        dir: tempfile::TempDir,
        owning: PathBuf,
        worktree: PathBuf,
    }

    impl MergeFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let owning = dir.path().join("owning");
            std::fs::create_dir_all(owning.join(".git")).expect("mk owning .git");
            std::fs::create_dir_all(owning.join("src")).expect("mk owning src");
            let worktree = dir.path().join("wt");
            std::fs::create_dir_all(worktree.join("src")).expect("mk wt src");
            std::fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").expect("mk wt .git");
            Self {
                owning: owning.canonicalize().expect("canon owning"),
                worktree: worktree.canonicalize().expect("canon wt"),
                dir,
            }
        }

        fn locks(&self) -> PathBuf {
            self.dir.path().join("locks")
        }

        /// Create `rel` under both roots (merged content exists on both sides)
        /// and return (worktree path, owning path).
        fn pair(&self, rel: &str) -> (PathBuf, PathBuf) {
            let wt = self.worktree.join(rel);
            let own = self.owning.join(rel);
            std::fs::write(&wt, b"fn f() {}\n").expect("write wt file");
            std::fs::write(&own, b"fn f() {}\n").expect("write owning file");
            (wt, own)
        }
    }

    #[test]
    fn merge_transfer_books_unpaid_intersect_merged_only() {
        // The wf-01 acceptance matrix in one ledger round-trip: the worker
        // leaves a.rs PAID, b.rs and c.rs unpaid; the merge carries a.rs and
        // b.rs but not c.rs (uncommitted work never merges). Exactly b.rs —
        // unpaid AND merged — books into the owning ledger.
        let fx = MergeFixture::new();
        let locks = fx.locks();
        let worker = Owner::new("claude", "sess-w", "w1");
        let lead = Owner::new("claude", "sess-l", "");
        let booking = rust_booking();
        let now = SystemTime::now();

        let (wt_a, own_a) = fx.pair("src/a.rs");
        let (wt_b, own_b) = fx.pair("src/b.rs");
        let (wt_c, _own_c) = fx.pair("src/c.rs");
        for f in [&wt_a, &wt_b, &wt_c] {
            assert!(matches!(
                acquire_in(&locks, f, &worker, &booking, now),
                Acquired::Ours
            ));
        }
        // The worker pays a.rs (diagnosed and delivered) before the merge.
        unlink_delivered_in(&locks, &fx.worktree, std::slice::from_ref(&wt_a));

        // What merged: a.rs and b.rs arrived in the owning tree; c.rs did not.
        let merged: std::collections::BTreeSet<PathBuf> = [
            own_a.canonicalize().expect("canon a"),
            own_b.canonicalize().expect("canon b"),
        ]
        .into_iter()
        .collect();
        merge_transfer_in(&locks, &fx.worktree, &fx.owning, &lead, &merged);

        assert_eq!(
            due_files_in(&locks, &fx.owning),
            vec![own_b.canonicalize().expect("canon b")],
            "exactly the unpaid-AND-merged file books into the owning ledger: \
             the paid file transfers nothing, the unmerged file transfers nothing"
        );
        // The transfer books under the merging identity.
        let owning_lock = root_lock_dir_in(&locks, &fx.owning);
        assert_eq!(
            read_owner_name(&owning_lock).expect("read owner"),
            Some(lead.file_name()),
            "the inherited debt belongs to the lead who merged"
        );
        // The worktree's own ledger is untouched — it retires with the
        // worktree, and a kept worktree's worker still owes its own gate.
        assert_eq!(
            due_files_in(&locks, &fx.worktree),
            vec![
                wt_b.canonicalize().expect("canon wt b"),
                wt_c.canonicalize().expect("canon wt c")
            ],
            "the transfer never mutates the worktree ledger"
        );
    }

    #[test]
    fn merge_transfer_of_a_paid_worktree_books_nothing() {
        // The paid-worker acceptance leg: an empty worktree ledger transfers
        // nothing no matter what merged — a paid landing arrives debt-free.
        let fx = MergeFixture::new();
        let locks = fx.locks();
        let lead = Owner::new("claude", "sess-l", "");
        let (_wt_a, own_a) = fx.pair("src/a.rs");
        let merged = std::collections::BTreeSet::from([own_a.canonicalize().expect("canon a")]);

        merge_transfer_in(&locks, &fx.worktree, &fx.owning, &lead, &merged);

        assert!(
            !root_lock_dir_in(&locks, &fx.owning).exists(),
            "a paid worktree's merge must not even create the owning lock dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn merge_transfer_books_through_aliased_spelling_into_canonical_ledger() {
        // The spelling pin on the worktree→owning mapping (wf-01 acceptance):
        // both roots handed to the transfer through symlink aliases must still
        // read the canonical worktree ledger and book canonical paths into the
        // canonical owning ledger — the same spellings the edit seam and the
        // diagnose serve key by (misc 193).
        let fx = MergeFixture::new();
        let locks = fx.locks();
        let worker = Owner::new("claude", "sess-w", "w1");
        let lead = Owner::new("claude", "sess-l", "");
        let booking = rust_booking();
        let now = SystemTime::now();

        let wt_alias = fx.dir.path().join("wt-alias");
        std::os::unix::fs::symlink(&fx.worktree, &wt_alias).expect("mk wt alias");
        let owning_alias = fx.dir.path().join("owning-alias");
        std::os::unix::fs::symlink(&fx.owning, &owning_alias).expect("mk owning alias");

        // The worker books canonically (the edit seam canonicalizes).
        let (wt_b, own_b) = fx.pair("src/b.rs");
        assert!(matches!(
            acquire_in(&locks, &wt_b, &worker, &booking, now),
            Acquired::Ours
        ));

        // The transfer is driven entirely through the ALIAS spellings.
        let merged = std::collections::BTreeSet::from([own_b.canonicalize().expect("canon b")]);
        merge_transfer_in(&locks, &wt_alias, &owning_alias, &lead, &merged);

        assert_eq!(
            due_files_in(&locks, &fx.owning),
            vec![own_b.canonicalize().expect("canon b")],
            "aliased root spellings still book the canonical owning ledger \
             with canonical file paths"
        );
    }

    // ── The board's lock-facts read surface (root-ownership stage 6) ────────

    #[test]
    fn facts_for_reads_owner_due_and_age_from_the_lock_dir() {
        let fx = Fixture::new();
        let f1 = fx.file("src/a.rs");
        let f2 = fx.file("src/b.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        // Unlocked → no facts (the board renders such a root as absent-of-lock).
        assert!(
            facts_for_in(&locks, &fx.root, now).is_none(),
            "an unlocked root has no lock facts"
        );

        assert!(matches!(
            acquire_in(&locks, &f1, &owner, &booking, now),
            Acquired::Ours
        ));
        assert!(matches!(
            acquire_in(&locks, &f2, &owner, &booking, now),
            Acquired::Ours
        ));

        // Read facts against a `now` after the acquisition (the owner file's
        // mtime is stamped during `acquire_in`, so a `now` captured before it
        // would read the activity as "in the future" — a test artifact, not a
        // real one; production reads a fresh wall clock).
        let after = now + Duration::from_secs(1);
        let facts = facts_for_in(&locks, &fx.root, after).expect("locked root has facts");
        assert_eq!(
            facts.owner.as_ref().map(Owner::file_name),
            Some(owner.file_name()),
            "the owner label is read from the owner-file name"
        );
        assert_eq!(facts.due, 2, "the due count is the touch-tree leaf count");
        assert!(facts.age.is_some(), "the last-activity age is readable");

        // Payment drains the due set; the lock (and its facts) survive.
        unlink_delivered_in(&locks, &fx.root, &[f1, f2]);
        let paid = facts_for_in(&locks, &fx.root, after).expect("paid lock still has facts");
        assert_eq!(paid.due, 0, "a paid lock reports zero due");
        assert_eq!(
            paid.owner.as_ref().map(Owner::file_name),
            Some(owner.file_name()),
            "the owner label survives payment (parole, not release)"
        );
    }

    #[test]
    fn list_locks_enumerates_every_lock_dir() {
        let fx = Fixture::new();
        // A second root beside the first, each with its own lock.
        let other = fx.dir.path().join("other");
        std::fs::create_dir_all(other.join(".git")).expect("mk other .git");
        let other = other.canonicalize().expect("canon other");
        std::fs::write(other.join("main.rs"), b"").expect("write");

        let owner_a = Owner::new("claude", "sess-a", "");
        let owner_b = Owner::new("claude", "sess-b", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        assert!(matches!(
            acquire_in(&locks, &fx.file("src/main.rs"), &owner_a, &booking, now),
            Acquired::Ours
        ));
        assert!(matches!(
            acquire_in(&locks, &other.join("main.rs"), &owner_b, &booking, now),
            Acquired::Ours
        ));

        let inventory = list_locks_in(&locks, now);
        assert_eq!(inventory.len(), 2, "both lock dirs are enumerated");
        // Facts pair with the encoded dir name; each carries its own owner label.
        let owners: std::collections::BTreeSet<String> = inventory
            .iter()
            .filter_map(|(_, f)| f.owner.as_ref().map(Owner::file_name))
            .collect();
        assert!(owners.contains(&owner_a.file_name()));
        assert!(owners.contains(&owner_b.file_name()));
    }

    #[cfg(unix)]
    #[test]
    fn facts_read_through_aliased_spelling() {
        // The spelling rule at the board's read seam (misc 193): the board reads
        // facts through `facts_for`, which canonicalizes the queried root. A
        // symlinked-prefix alias of the root must read the SAME canonical lock dir
        // the edit seam booked under, or a held root would render as unlocked. A
        // green real-dir run masks this — the symlink alias is the regression pin.
        let fx = Fixture::new();
        let alias = fx.dir.path().join("alias");
        std::os::unix::fs::symlink(&fx.root, &alias).expect("mk symlink");
        let via_alias = alias.join("src/main.rs");
        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        assert!(matches!(
            acquire_in(&locks, &via_alias, &owner, &booking, now),
            Acquired::Ours
        ));

        // Reading facts for the ALIASED root path resolves to the canonical lock
        // dir — the held root renders as held, not unlocked.
        let facts =
            facts_for_in(&locks, &alias.canonicalize().unwrap_or(alias), now).expect("facts");
        assert_eq!(
            facts.owner.as_ref().map(Owner::file_name),
            Some(owner.file_name()),
            "the aliased read resolves to the canonical held lock"
        );
        assert_eq!(facts.due, 1, "the canonical ledger's due count");
    }

    // ── Nested-root resolution: innermost covered root wins (stage 6) ───────

    #[test]
    fn edit_in_nested_inner_root_books_inner_not_outer() {
        // The nested-root pin (deliverable 4): an inner covered root (its own
        // `.git` marker) nested inside an outer repo. An edit inside the inner
        // root books against the INNER kitchen; the outer root's lock is untouched.
        // The lock, the ledger, and the briefing all agree which kitchen the
        // nested path belongs to — the same innermost resolution queries use.
        let fx = Fixture::new();
        // An inner repo at `<outer>/inner/` with its own marker.
        let inner = fx.root.join("inner");
        std::fs::create_dir_all(inner.join(".git")).expect("mk inner .git");
        std::fs::create_dir_all(inner.join("src")).expect("mk inner src");
        let inner = inner.canonicalize().expect("canon inner");
        let inner_file = inner.join("src/lib.rs");
        std::fs::write(&inner_file, b"").expect("write inner file");

        let owner = Owner::new("claude", "sess-a", "");
        let booking = rust_booking();
        let now = SystemTime::now();
        let locks = fx.locks();

        // The edit resolves to the INNER root (nearest enclosing marker).
        assert_eq!(
            resolve_lock_root(&inner_file).as_deref(),
            Some(inner.as_path()),
            "the nested edit resolves to the innermost covered root"
        );

        assert!(matches!(
            acquire_in(&locks, &inner_file, &owner, &booking, now),
            Acquired::Ours
        ));

        // The inner kitchen booked the file.
        assert_eq!(
            due_files_in(&locks, &inner),
            vec![inner_file],
            "the inner kitchen books the nested edit"
        );
        assert!(has_debt_in(&locks, &inner), "inner has debt");

        // The OUTER root's lock is untouched — no dir, no owner, no debt.
        assert!(
            !root_lock_dir_in(&locks, &fx.root).exists(),
            "the outer root's lock is untouched by a nested-inner edit"
        );
        assert!(
            facts_for_in(&locks, &fx.root, now).is_none(),
            "the outer root renders as unlocked"
        );
        assert!(!has_debt_in(&locks, &fx.root), "outer has no debt");
    }

    /// A tempdir carrying several sibling repo roots (each with a `.git`
    /// marker) around one shared `locks/` base — the bug-121 multi-kitchen
    /// enumeration/policy fixture.
    struct MultiFixture {
        dir: tempfile::TempDir,
    }

    impl MultiFixture {
        fn new(repos: &[&str]) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            for name in repos {
                let root = dir.path().join(name);
                std::fs::create_dir_all(root.join(".git")).expect("mk .git");
                std::fs::create_dir_all(root.join("src")).expect("mk src");
            }
            Self { dir }
        }

        fn locks(&self) -> PathBuf {
            self.dir.path().join("locks")
        }

        fn root(&self, name: &str) -> PathBuf {
            self.dir
                .path()
                .join(name)
                .canonicalize()
                .expect("canon root")
        }

        /// Book a covered edit in `repo` under `owner`, asserting admission.
        /// Returns the booked file's canonical path.
        fn book(&self, repo: &str, rel: &str, owner: &Owner) -> PathBuf {
            let file = self.root(repo).join(rel);
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).expect("mk parent");
            }
            std::fs::write(&file, b"").expect("write file");
            assert!(
                matches!(
                    acquire_in(
                        &self.locks(),
                        &file,
                        owner,
                        &rust_booking(),
                        SystemTime::now()
                    ),
                    Acquired::Ours
                ),
                "the booking edit must be admitted"
            );
            file
        }

        /// A cwd outside every repo root (no repository marker).
        fn scratch(&self) -> PathBuf {
            let p = self.dir.path().join("scratch");
            std::fs::create_dir_all(&p).expect("mk scratch");
            p
        }
    }

    #[test]
    fn booking_writes_the_root_record_and_debtor_roots_finds_it() {
        let fx = MultiFixture::new(&["repo-a"]);
        let a = Owner::new("claude", "sess-a", "");
        let file = fx.book("repo-a", "src/main.rs", &a);

        assert_eq!(
            debtor_roots_in(&fx.locks()),
            vec![fx.root("repo-a")],
            "the booked kitchen is enumerable via its root record"
        );

        // Payment empties the ledger — the kitchen leaves the debtor list
        // (the lock dir itself survives: payment is parole, not release).
        let _ = unlink_delivered_in(&fx.locks(), &fx.root("repo-a"), &[file]);
        assert!(
            debtor_roots_in(&fx.locks()).is_empty(),
            "a paid kitchen is not a debtor"
        );
    }

    #[test]
    fn debtor_roots_skips_recordless_lock_dirs_and_rebooking_heals() {
        let fx = MultiFixture::new(&["repo-a"]);
        let a = Owner::new("claude", "sess-a", "");
        fx.book("repo-a", "src/main.rs", &a);

        // A pre-record lock dir (booked before the fix): no `.root`, so the
        // enumeration cannot recover the kitchen — invisible exactly as
        // pre-fix, never a guess.
        let lock_dir = root_lock_dir_in(&fx.locks(), &fx.root("repo-a"));
        std::fs::remove_file(lock_dir.join(".root")).expect("drop record");
        assert!(
            debtor_roots_in(&fx.locks()).is_empty(),
            "a recordless lock dir is skipped, not guessed at"
        );

        // The next booking self-heals the record.
        fx.book("repo-a", "src/other.rs", &a);
        assert_eq!(
            debtor_roots_in(&fx.locks()),
            vec![fx.root("repo-a")],
            "re-booking lands the record and the kitchen is enumerable again"
        );
    }

    #[test]
    fn bare_serve_roots_anchored_pulls_same_owner_kitchens_only() {
        let fx = MultiFixture::new(&["repo-a", "repo-b", "repo-c"]);
        let ours = Owner::new("claude", "sess-a", "");
        let theirs = Owner::new("claude", "sess-b", "");
        fx.book("repo-a", "src/main.rs", &ours);
        fx.book("repo-b", "src/main.rs", &ours);
        fx.book("repo-c", "src/main.rs", &theirs);

        // Anchored in repo-a (our lock): our other indebted kitchen rides
        // along; the other identity's kitchen is never pulled.
        assert_eq!(
            bare_serve_roots_in(&fx.locks(), &fx.root("repo-a")),
            vec![fx.root("repo-a"), fx.root("repo-b")],
            "the anchored bare serve pulls the anchor owner's kitchens only"
        );
    }

    #[test]
    fn bare_serve_roots_unanchored_single_owner_serves_the_debt() {
        let fx = MultiFixture::new(&["repo-a", "repo-b"]);
        let a = Owner::new("claude", "sess-a", "");
        fx.book("repo-a", "src/main.rs", &a);
        fx.book("repo-b", "src/main.rs", &a);

        // cwd outside ANY root, all debt held by one identity: attribution is
        // unambiguous — the ledger, not the cwd, is the truth (bug 121 pin).
        assert_eq!(
            bare_serve_roots_in(&fx.locks(), &fx.scratch()),
            vec![fx.root("repo-a"), fx.root("repo-b")],
            "an unanchored bare serve with one debtor identity serves its debt"
        );
    }

    #[test]
    fn bare_serve_roots_unanchored_ambiguous_serves_nothing() {
        let fx = MultiFixture::new(&["repo-a", "repo-b"]);
        fx.book("repo-a", "src/main.rs", &Owner::new("claude", "sess-a", ""));
        fx.book("repo-b", "src/main.rs", &Owner::new("claude", "sess-b", ""));

        // Two identities hold debt and no anchor says which is the caller's:
        // no path-algebraic fact can attribute, so nothing is pulled.
        assert!(
            bare_serve_roots_in(&fx.locks(), &fx.scratch()).is_empty(),
            "an unanchored bare serve never guesses between identities"
        );
    }

    /// Pins the bug-128 fix: with the identity-aware [`vetted_serve_roots_in`]
    /// the caller's own kitchen is always found, even when an unrelated identity
    /// holds debt in a sibling root.
    ///
    /// Bug 128 geometry (daemon `24aafb70`, 19:45:00Z): caller stands in an
    /// UNLOCKED root, its own debt lives in `repo-ours`, and an unrelated
    /// identity (`theirs`) holds debt in `repo-theirs`. The identity-free
    /// [`bare_serve_roots_in`] could not disambiguate (two owners → ambiguous
    /// → `[repo-cwd]` only). The hook, which has identity, now calls
    /// [`vetted_serve_roots_in`] and correctly serves `repo-ours` while silently
    /// excluding `repo-theirs`.
    #[test]
    fn bare_serve_roots_unanchored_foreign_debt_hides_the_callers_own_kitchen() {
        let fx = MultiFixture::new(&["repo-cwd", "repo-ours", "repo-theirs"]);
        let ours = Owner::new("claude", "sess-a", "");
        let theirs = Owner::new("claude", "sess-a", "agent-b");
        fx.book("repo-ours", "src/main.rs", &ours);
        fx.book("repo-theirs", "src/main.rs", &theirs);

        // Identity-free path: still ambiguous — unchanged for hookless callers.
        assert_eq!(
            bare_serve_roots_in(&fx.locks(), &fx.root("repo-cwd")),
            vec![fx.root("repo-cwd")],
            "identity-free path: two owners → serves only the cwd root (hookless posture unchanged)"
        );

        // Identity-aware path (the hook uses this): the caller's own kitchen is
        // served; the foreign debt in repo-theirs is excluded without a deny.
        assert_eq!(
            vetted_serve_roots_in(&fx.locks(), &fx.root("repo-cwd"), &ours),
            vec![fx.root("repo-cwd"), fx.root("repo-ours")],
            "bug 128 fix: vetted_serve_roots finds the caller's own kitchen even with foreign debt"
        );
    }

    #[test]
    fn bare_serve_roots_includes_the_unlocked_cwd_root_first() {
        let fx = MultiFixture::new(&["repo-a", "repo-b"]);
        let a = Owner::new("claude", "sess-a", "");
        fx.book("repo-b", "src/main.rs", &a);

        // The cwd's root is unlocked (nothing edited there) — it still leads
        // the served set (the single-root contract), and the one debtor
        // identity's kitchen rides along (the bug-121 sighting's shape).
        assert_eq!(
            bare_serve_roots_in(&fx.locks(), &fx.root("repo-a")),
            vec![fx.root("repo-a"), fx.root("repo-b")],
            "the unlocked cwd root leads; the sibling kitchen's debt is served"
        );
    }

    #[test]
    fn bare_serve_roots_never_attributes_ownerless_debt() {
        let fx = MultiFixture::new(&["repo-a", "repo-b"]);
        let a = Owner::new("claude", "sess-a", "");
        fx.book("repo-b", "src/main.rs", &a);

        // Strip the owner file: a crashed acquisition's ownerless debt has no
        // identity to attribute — never pulled from elsewhere.
        let lock_dir = root_lock_dir_in(&fx.locks(), &fx.root("repo-b"));
        let owner_name = read_owner_name(&lock_dir)
            .expect("read owner")
            .expect("owner present");
        std::fs::remove_file(lock_dir.join(owner_name)).expect("drop owner");

        assert_eq!(
            bare_serve_roots_in(&fx.locks(), &fx.root("repo-a")),
            vec![fx.root("repo-a")],
            "ownerless debt is never attributed to the caller"
        );
    }

    // ── vetted_serve_roots_in tests (bugs 124/128) ───────────────────────

    /// Bug 124: unanchored caller, sole debtor is a stranger.
    ///
    /// Acceptance test outcome 1: the vetted set has no debtor kitchens for
    /// the caller (the stranger's kitchen is excluded), so the serve honestly
    /// answers `[no edited files]`. No deny, no claim invitation.
    #[test]
    fn vetted_serve_roots_sole_foreign_debtor_yields_empty_extras() {
        let fx = MultiFixture::new(&["repo-cwd", "repo-stranger"]);
        let caller = Owner::new("claude", "sess-caller", "");
        let stranger = Owner::new("claude", "sess-stranger", "");
        fx.book("repo-stranger", "src/main.rs", &stranger);

        // The caller's vetted set is just the cwd root — no debtor kitchens.
        assert_eq!(
            vetted_serve_roots_in(&fx.locks(), &fx.root("repo-cwd"), &caller),
            vec![fx.root("repo-cwd")],
            "bug 124: sole foreign debtor is excluded; caller gets only their cwd root"
        );
    }

    /// Bug 128: caller owns debt in one root, a stranger in another.
    ///
    /// Acceptance test outcome 2: the vetted set includes the caller's own
    /// kitchen (`repo-ours`) and excludes the stranger's (`repo-theirs`).
    #[test]
    fn vetted_serve_roots_includes_callers_own_kitchen_excludes_foreign() {
        let fx = MultiFixture::new(&["repo-cwd", "repo-ours", "repo-theirs"]);
        let caller = Owner::new("claude", "sess-a", "");
        let stranger = Owner::new("claude", "sess-a", "agent-b");
        fx.book("repo-ours", "src/main.rs", &caller);
        fx.book("repo-theirs", "src/main.rs", &stranger);

        assert_eq!(
            vetted_serve_roots_in(&fx.locks(), &fx.root("repo-cwd"), &caller),
            vec![fx.root("repo-cwd"), fx.root("repo-ours")],
            "bug 128: caller's own kitchen is served; the stranger's is excluded silently"
        );
    }

    /// Acceptance test outcome 4 (hookless / field absent): `bare_serve_roots_in`
    /// is unchanged. The vetted function is only called from the hook; the
    /// identity-free path stays byte-identical to the pre-fix posture.
    #[test]
    fn vetted_serve_roots_anchored_cwd_matches_bare_serve_for_same_owner() {
        let fx = MultiFixture::new(&["repo-a", "repo-b"]);
        let owner = Owner::new("claude", "sess-a", "");
        fx.book("repo-a", "src/main.rs", &owner);
        fx.book("repo-b", "src/main.rs", &owner);

        // When the caller owns all the debt and is anchored, both functions
        // agree — the vetted path is strictly a superset, never narrower.
        assert_eq!(
            vetted_serve_roots_in(&fx.locks(), &fx.root("repo-a"), &owner),
            bare_serve_roots_in(&fx.locks(), &fx.root("repo-a")),
            "vetted and bare paths agree when the caller owns all debt and is anchored"
        );
    }

    /// Acceptance test outcome 5 (wire / serde-default): `vetted_roots` absent
    /// from a `pre-tool/editing-stop` request deserialises without error and
    /// yields an empty vec, which the daemon treats as "no override" and falls
    /// back to `bare_serve_roots`.
    #[test]
    fn hook_request_pre_tool_editing_stop_vetted_roots_defaults_to_empty() {
        use crate::hook::HookRequest;
        // Old hook: no vetted_roots field.
        let json = r#"{"method": "pre-tool/editing-stop", "session_id": "sess-x"}"#;
        let req: HookRequest = serde_json::from_str(json).expect("deserialise");
        let HookRequest::PreToolDoneEditingPrepare { vetted_roots, .. } = req else {
            unreachable!("expected PreToolDoneEditingPrepare");
        };
        assert!(
            vetted_roots.is_empty(),
            "absent vetted_roots field defaults to an empty vec"
        );

        // New hook: vetted_roots present.
        let json = r#"{"method": "pre-tool/editing-stop", "vetted_roots": ["/a", "/b"]}"#;
        let req: HookRequest = serde_json::from_str(json).expect("deserialise with vetted_roots");
        let HookRequest::PreToolDoneEditingPrepare { vetted_roots, .. } = req else {
            unreachable!("expected PreToolDoneEditingPrepare");
        };
        assert_eq!(
            vetted_roots,
            vec![
                std::path::PathBuf::from("/a"),
                std::path::PathBuf::from("/b")
            ],
            "present vetted_roots round-trips through serde"
        );
    }
}
