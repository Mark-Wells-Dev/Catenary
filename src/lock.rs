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

/// The touch-file path for a due file, mirrored under `dir/` with a `.lock`
/// suffix: `dir/<root-relative-path>.lock`.
///
/// Falls back to the flattened absolute path when the file is not under `root`
/// (defensive — the caller resolves the root first, so this should not happen
/// in practice).
fn touch_file(lock_dir: &Path, root: &Path, file: &Path) -> PathBuf {
    let rel = file.strip_prefix(root).unwrap_or(file);
    let mut name = rel.as_os_str().to_os_string();
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

/// Resolves the root a file belongs to for locking purposes, or `None` when the
/// file is in genuinely-foreign territory (no repository marker above it).
///
/// Rides the same gate the query auto-mount uses
/// ([`crate::companions::enclosing_worktree_root`], repo-marker root
/// resolution): edits in `/tmp`, scratch dirs, or anywhere outside a VCS
/// checkout resolve to `None`, so no lock is taken. The root is canonicalized so
/// its encoding matches the daemon's canonical roots.
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

/// Unlinks the touch files for a set of delivered files from a root's ledger
/// under `locks_base` (delivery deletes — the daemon knows which files served).
///
/// Called daemon-side on the diagnostics-delivery seam. Removing a file's touch
/// entry marks it paid; when `dir/` empties, the lock is paid and the idle
/// countdown starts. **Does not remove the lock** — payment is parole, not
/// release. Prunes now-empty mirrored subdirectories so `dir/` genuinely empties.
/// Best-effort: a failed unlink is ignored (the reaper's empty check still holds).
pub fn unlink_delivered_in(locks_base: &Path, root: &Path, files: &[PathBuf]) {
    let lock_dir = root_lock_dir_in(locks_base, root);
    if !lock_dir.exists() {
        return;
    }
    let base = ledger_dir(&lock_dir);
    for file in files {
        let touch = touch_file(&lock_dir, root, file);
        let _ = std::fs::remove_file(&touch);
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
}

/// Production wrapper for [`unlink_delivered_in`] resolving the base through
/// [`locks_dir`].
pub fn unlink_delivered(root: &Path, files: &[PathBuf]) {
    unlink_delivered_in(&locks_dir(), root, files);
}

/// Retires a root's lock entirely under `locks_base`: removes
/// `<locks_base>/<encoded-root>/` (owner file, ledger, and all) — release leg 2
/// (root retirement).
///
/// Called when a worktree lands / is removed / vanishes: the lock and its ledger
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
}
