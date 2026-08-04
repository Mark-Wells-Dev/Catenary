// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The mtime trail: how the supplemental watch probe finds directories it has
//! never seen without walking the tree (bug 146, stage 2).
//!
//! The probe's marker leg (`**/Cargo.toml`, `**/.lattice.toml`) has to answer
//! *where could this name be?* — and the registration IS the authorization to
//! look. The affordable way to look is the trail a filesystem already keeps: a
//! **directory's mtime advances exactly when its direct children change**
//! (create / delete / rename). Content edits do not move it — and they do not
//! need to, since every marker is stat'd by name anyway.
//!
//! So each nudge:
//!
//! - `lstat` the known directory set — O(known dirs), no traversal;
//! - `readdir` **only** the directories whose mtime advanced;
//! - newly discovered directories join the set and are enumerated in turn, so a
//!   new subtree unfolds down its own path and a new nested marker is found at
//!   the very next nudge.
//!
//! Steady state is therefore N lstats and zero readdirs.
//!
//! # Descent posture
//!
//! Standard prune posture with the tracked-beats-hidden exception (`998c9bc`):
//! full filters-off recursion would re-admit `target/`, the budget the bug-143
//! ruling protected. Genuinely excluded trees stay served by the `baseUri` and
//! literal legs, which go where search never goes.
//!
//! # Trust: tested, never assumed
//!
//! Directory mtimes are not universal. They are solid on local
//! ext4/btrfs/xfs/zfs/APFS/NTFS, **broken** on the FAT family (directory
//! timestamps do not track child changes), and **cache-delayed** on
//! NFS/SMB/9p. Git faced exactly this and answered with
//! `core.untrackedCache` + `git update-index --test-untracked-cache`: verify
//! the filesystem's semantics empirically before relying on them, and degrade
//! to full readdir otherwise. This module adopts the pattern with two
//! improvements:
//!
//! 1. **Per device, not per workspace.** The classification keys on `st_dev`,
//!    which rides every `lstat` the trail already makes. A mount swap surfaces
//!    as an unclassified device in those same stats; a submount inside the tree
//!    (an NFS mount under an ext4 workspace) is classified separately and
//!    automatically.
//! 2. **Continuously recertified, never a stored verdict.** Git's test is
//!    one-shot — a repository moved across filesystems keeps its stale verdict,
//!    a known footgun. Here the edit path is a perpetual canary: a booked write
//!    we KNOW changes a directory entry is compared against that directory's
//!    own mtime at the next scan, using data both sides already produce (the
//!    booking supplies ground truth, the trail's lstats supply the
//!    observation — zero marginal syscalls beyond confirming the write landed).
//!
//! **Only entry-changing writes are valid canaries** — creations and
//! rename-class writes. An in-place overwrite (a shell redirect over an
//! existing file) does not move the parent directory's mtime on ANY
//! filesystem, so counting it would false-demote every healthy device on its
//! first redirect write. See [`WatchTrail::note_entry_changing_write`] for the
//! caller-side predicate.
//!
//! Hysteresis is asymmetric: **one** failed valid canary demotes a device
//! immediately; promotion requires [`PROMOTION_DEMONSTRATIONS`] fresh
//! positives. Degradation is always toward more I/O, never toward a wrong
//! answer — an unclassified or degraded device simply readdirs its known
//! directories unconditionally.
//!
//! # The granularity race
//!
//! A directory changed within the same timestamp tick as our observation would
//! read as unchanged forever after. Racy-git's answer applies: any directory
//! whose observed mtime **ties or exceeds the scan's own boundary** is
//! re-verified by readdir on the next scan rather than trusted for equality.
//!
//! # The uncovered corner, stated honestly
//!
//! A reformat onto the SAME device number combined with a read-only workspace
//! (no writes ⇒ no canaries, no device change ⇒ no identity trip). Its ceiling
//! is low by construction: a lying directory mtime can only cause LATE
//! DISCOVERY of new entries in that directory. Deletions and content changes
//! ride direct per-path stats that never consult a directory mtime, and the
//! diagnose round's wholesale sweep re-syncs everything it touches regardless.
//! Bounded staleness, never a wrong answer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use ignore::WalkBuilder;

use crate::bridge::filesystem_manager::{mtime_nanos, stat_with_retry};

/// Fresh positive canaries required to promote a device to trusted.
///
/// Deliberately larger than one: a single advancing directory mtime can be
/// coincidental (an unrelated concurrent write in the same directory), while
/// three independent booked writes each moving their parent is the behavior
/// the trail actually depends on. Demotion needs no such margin — one
/// demonstrated failure is proof.
const PROMOTION_DEMONSTRATIONS: u8 = 3;

/// A device's standing on the trail's trust ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Never demonstrated either way — runs degraded until proven. A freshly
    /// swapped mount lands here, which is the point.
    Unclassified,
    /// Demonstrated `PROMOTION_DEMONSTRATIONS` times that a booked
    /// entry-changing write advances its directory's mtime.
    Trusted,
    /// Demonstrated at least once that it does NOT. Readdirs unconditionally.
    Degraded,
}

/// One device's ledger entry.
#[derive(Debug, Clone, Copy)]
struct DeviceState {
    verdict: Verdict,
    /// Consecutive positive demonstrations since the last demotion.
    positives: u8,
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            verdict: Verdict::Unclassified,
            positives: 0,
        }
    }
}

/// Per-device trust in directory-mtime semantics.
#[derive(Debug, Default)]
struct TrustLedger {
    devices: HashMap<u64, DeviceState>,
}

impl TrustLedger {
    /// Whether the trail may skip readdir for a directory on this device.
    fn is_trusted(&self, dev: u64) -> bool {
        self.devices
            .get(&dev)
            .is_some_and(|s| s.verdict == Verdict::Trusted)
    }

    /// Records one canary outcome. `advanced` is whether the directory's mtime
    /// moved past its pre-write value for a write we KNOW changed an entry.
    fn record(&mut self, dev: u64, advanced: bool) {
        let state = self.devices.entry(dev).or_default();
        if advanced {
            state.positives = state.positives.saturating_add(1);
            if state.positives >= PROMOTION_DEMONSTRATIONS {
                state.verdict = Verdict::Trusted;
            }
        } else {
            // One demonstrated failure is proof. Promotion starts over from
            // zero — a stale positive streak must not survive the evidence
            // that the device lies.
            state.verdict = Verdict::Degraded;
            state.positives = 0;
        }
    }

    /// This device's current verdict.
    #[cfg(test)]
    fn verdict(&self, dev: u64) -> Verdict {
        self.devices
            .get(&dev)
            .map_or(Verdict::Unclassified, |s| s.verdict)
    }
}

/// One known directory's last observation.
///
/// The owning device is deliberately NOT cached here: it is re-read from every
/// scan's `lstat`, which is what makes a mount swap surface as an unclassified
/// device in the very stats the trail is already making.
#[derive(Debug, Clone, Copy)]
struct DirState {
    /// mtime at the last scan.
    mtime: i64,
    /// The racy-git flag: this mtime tied or exceeded the scan boundary that
    /// observed it, so equality next time proves nothing and the directory is
    /// re-verified by readdir.
    racy: bool,
}

/// A booked entry-changing write awaiting its verdict.
#[derive(Debug, Clone)]
struct Canary {
    /// The written path — stat'd at evaluation time to confirm the write
    /// actually landed, so a scan that races ahead of the tool cannot
    /// false-demote a healthy device.
    target: PathBuf,
    /// The directory's mtime as of the last scan BEFORE the write.
    pre_dir_mtime: i64,
}

/// One root's directory trail.
#[derive(Debug, Default)]
struct RootTrail {
    /// Known directories (absolute) and their last observation.
    dirs: HashMap<PathBuf, DirState>,
    /// Pending canaries, keyed by the written file's parent directory.
    canaries: HashMap<PathBuf, Canary>,
    /// Whether this root has been seeded (distinguishes "cold" from "a root
    /// whose directories all vanished").
    seeded: bool,
}

/// The trail registry: one [`RootTrail`] per workspace root, one shared trust
/// ledger across roots (devices are a machine property, not a project one).
#[derive(Debug, Default)]
pub struct WatchTrail {
    roots: Mutex<HashMap<PathBuf, Arc<Mutex<RootTrail>>>>,
    trust: Mutex<TrustLedger>,
    /// Directories readdir'd since process start — the observable the
    /// "steady state does no traversal" guards read.
    readdirs: std::sync::atomic::AtomicU64,
}

impl WatchTrail {
    /// Refreshes `root`'s trail and returns its known directories as
    /// **root-relative** paths (the marker leg's candidate set; the root itself
    /// is the empty path).
    ///
    /// Does the trail's I/O: one `lstat` per known directory, plus a `readdir`
    /// for each directory whose mtime advanced, tied the last scan boundary, or
    /// sits on a device that has not demonstrated trustworthy mtimes. A cold
    /// root is seeded by one enumeration in standard prune posture — the only
    /// traversal the trail ever performs for a root that keeps existing.
    pub(crate) fn refresh(&self, root: &Path) -> Vec<PathBuf> {
        let trail = {
            let mut roots = self.roots.lock().unwrap_or_else(PoisonError::into_inner);
            let trail = Arc::clone(roots.entry(root.to_path_buf()).or_default());
            drop(roots);
            trail
        };
        let mut trail = trail.lock().unwrap_or_else(PoisonError::into_inner);
        self.scan(root, &mut trail);
        let mut dirs: Vec<PathBuf> = trail
            .dirs
            .keys()
            .filter_map(|dir| dir.strip_prefix(root).ok().map(Path::to_path_buf))
            .collect();
        drop(trail);
        dirs.sort_unstable();
        dirs.dedup();
        dirs
    }

    /// Books an **entry-changing** write as a canary for its directory's device.
    ///
    /// The caller's predicate (`hook_router::record_covered_write`) is the
    /// scoped canary set: a **creation** (the target did not exist when the
    /// write was recorded) or a **rename-class** write (host `Edit`/`Write` are
    /// atomic-rename per bug 34's evidence; `sed -i` renames over). An in-place
    /// overwrite is NOT booked here — directory mtimes do not move for it on
    /// any filesystem, so it would false-demote every healthy device.
    ///
    /// A write into a directory the trail does not know yet books nothing:
    /// there is no pre-write observation to compare against, and the directory
    /// will be discovered on its own.
    ///
    /// (Seam noted for later unification: misc 230's write fingerprints record
    /// the same pre-state this derives from baseline/disk absence. When that
    /// machinery lands, the creation half of this predicate should read the
    /// fingerprint rather than re-deriving it.)
    pub(crate) fn note_entry_changing_write(&self, path: &Path) {
        let Some(dir) = path.parent() else {
            return;
        };
        let roots = self.roots.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(trail) = roots
            .iter()
            .find(|(root, _)| dir.starts_with(root.as_path()))
            .map(|(_, trail)| Arc::clone(trail))
        else {
            return;
        };
        drop(roots);
        let mut trail = trail.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(state) = trail.dirs.get(dir).copied() else {
            return;
        };
        // First booking wins: the earliest pre-write mtime is the strictest
        // comparison, and a second booking before the scan adds nothing.
        trail
            .canaries
            .entry(dir.to_path_buf())
            .or_insert_with(|| Canary {
                target: path.to_path_buf(),
                pre_dir_mtime: state.mtime,
            });
    }

    /// Drops a root's trail (root retired / unpinned).
    pub(crate) fn forget_root(&self, root: &Path) {
        self.roots
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(root);
    }

    /// One scan pass. See the module docs for the rule set.
    fn scan(&self, root: &Path, trail: &mut RootTrail) {
        let boundary = now_nanos();
        let tracked = crate::tracked::TrackedHidden::new();

        if !trail.seeded {
            trail.seeded = true;
            self.discover(root, root, &tracked, boundary, trail);
            return;
        }

        let known: Vec<PathBuf> = trail.dirs.keys().cloned().collect();
        let mut newly: Vec<PathBuf> = Vec::new();
        for dir in known {
            let Some(metadata) = stat_with_retry(&dir) else {
                // Gone from disk. Its markers are condemned by the marker leg's
                // own per-path stats; the directory simply leaves the set.
                trail.dirs.remove(&dir);
                trail.canaries.remove(&dir);
                continue;
            };
            let dev = device_of(&metadata);
            let mtime = mtime_nanos(&metadata);
            let previous = trail.dirs.get(&dir).copied();

            self.settle_canary(trail, &dir, dev, mtime);

            let trusted = self
                .trust
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_trusted(dev);
            let changed = previous.is_none_or(|p| p.racy || p.mtime != mtime) || !trusted;
            trail.dirs.insert(
                dir.clone(),
                DirState {
                    mtime,
                    racy: mtime >= boundary,
                },
            );
            if changed {
                for child in self.child_dirs(&dir, &tracked) {
                    if !trail.dirs.contains_key(&child) {
                        newly.push(child);
                    }
                }
            }
        }
        // A new subtree unfolds down its own path: each newly discovered
        // directory is enumerated whole, so a nested marker several levels down
        // is known by the end of THIS scan.
        for dir in newly {
            self.discover(root, &dir, &tracked, boundary, trail);
        }
    }

    /// Evaluates a pending canary for `dir`, if any, against its observed
    /// mtime.
    ///
    /// The write is confirmed landed first (the target's own mtime must have
    /// moved past the pre-write directory observation). Without that check a
    /// scan racing ahead of the tool would read "directory unchanged" and
    /// demote a perfectly healthy device; with it, an unlanded write simply
    /// stays pending.
    fn settle_canary(&self, trail: &mut RootTrail, dir: &Path, dev: u64, dir_mtime: i64) {
        let Some(canary) = trail.canaries.get(dir).cloned() else {
            return;
        };
        let Some(target) = stat_with_retry(&canary.target) else {
            // The write never materialized (a resolved write whose command
            // failed). No ground truth, no verdict.
            trail.canaries.remove(dir);
            return;
        };
        if mtime_nanos(&target) <= canary.pre_dir_mtime {
            // Not landed yet — keep waiting rather than judge the device.
            return;
        }
        trail.canaries.remove(dir);
        self.trust
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(dev, dir_mtime > canary.pre_dir_mtime);
    }

    /// Enumerates `dir` and every directory beneath it (standard prune posture)
    /// into the trail.
    fn discover(
        &self,
        root: &Path,
        dir: &Path,
        tracked: &Arc<crate::tracked::TrackedHidden>,
        boundary: i64,
        trail: &mut RootTrail,
    ) {
        let _ = root;
        self.readdirs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut builder = WalkBuilder::new(dir);
        builder.git_ignore(true);
        crate::tracked::apply_hidden_posture(&mut builder, dir, true, tracked);
        for entry in builder.build().flatten() {
            if !entry.file_type().is_some_and(|ft| ft.is_dir()) {
                continue;
            }
            let path = entry.path().to_path_buf();
            let Some(metadata) = stat_with_retry(&path) else {
                continue;
            };
            let mtime = mtime_nanos(&metadata);
            trail.dirs.insert(
                path,
                DirState {
                    mtime,
                    racy: mtime >= boundary,
                },
            );
        }
    }

    /// The direct child directories of `dir` under the standard prune posture.
    fn child_dirs(&self, dir: &Path, tracked: &Arc<crate::tracked::TrackedHidden>) -> Vec<PathBuf> {
        self.readdirs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut builder = WalkBuilder::new(dir);
        builder.max_depth(Some(1)).git_ignore(true);
        crate::tracked::apply_hidden_posture(&mut builder, dir, true, tracked);
        builder
            .build()
            .flatten()
            .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_dir()))
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| path.as_path() != dir)
            .collect()
    }

    /// Directories readdir'd since process start (test observable for the
    /// "steady state traverses nothing" guards).
    #[cfg(test)]
    fn readdir_count(&self) -> u64 {
        self.readdirs.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// This device's verdict (test observable for the trust-lifecycle guards).
    #[cfg(test)]
    fn verdict_of(&self, dev: u64) -> Verdict {
        self.trust
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .verdict(dev)
    }

    /// Records a canary outcome directly (test seam for the ledger's
    /// hysteresis, without staging a filesystem that lies).
    #[cfg(test)]
    fn record_canary_for_test(&self, dev: u64, advanced: bool) {
        self.trust
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(dev, advanced);
    }
}

/// The owning device of a stat result — the trust key.
///
/// Unix reads `st_dev` (`MetadataExt` is safe under `forbid(unsafe_code)`).
/// Elsewhere there is no device identity to key on, so everything shares
/// device `0` and stays unclassified — i.e. permanently degraded, which is the
/// correct answer for a platform whose directory-mtime semantics we cannot
/// certify.
#[cfg(unix)]
fn device_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.dev()
}

/// Non-unix stand-in: one unclassified device (see the unix arm).
#[cfg(not(unix))]
const fn device_of(_metadata: &std::fs::Metadata) -> u64 {
    0
}

/// Wall-clock nanoseconds — the scan boundary the racy rule compares against,
/// in the same epoch as [`mtime_nanos`].
fn now_nanos() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(i64::MAX, |d| {
            i64::try_from(d.as_nanos()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    /// Sets a directory's mtime far in the past, so a later change is
    /// unambiguous and the racy flag is off.
    fn age(path: &Path) {
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old))
            .expect("set dir mtime");
    }

    /// The device under the tempdir (every fixture path shares it).
    fn device_under(path: &Path) -> u64 {
        device_of(&std::fs::metadata(path).expect("stat"))
    }

    #[test]
    fn cold_root_is_seeded_with_every_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/inner")).expect("mkdir");
        std::fs::create_dir(root.join("docs")).expect("mkdir");

        let trail = WatchTrail::default();
        let dirs = trail.refresh(root);

        assert!(dirs.contains(&PathBuf::new()), "the root itself: {dirs:?}");
        assert!(dirs.contains(&PathBuf::from("src")), "{dirs:?}");
        assert!(dirs.contains(&PathBuf::from("src/inner")), "{dirs:?}");
        assert!(dirs.contains(&PathBuf::from("docs")), "{dirs:?}");
    }

    #[test]
    fn a_new_nested_directory_is_discovered_at_the_next_scan() {
        // The whole point of the trail: a marker created in a brand-new nested
        // directory is in the candidate set at the very next nudge, with no
        // wholesale walk and nothing feeding the probe from outside.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir(root.join("crates")).expect("mkdir");

        let trail = WatchTrail::default();
        let first = trail.refresh(root);
        assert!(!first.contains(&PathBuf::from("crates/new")), "{first:?}");

        std::fs::create_dir_all(root.join("crates/new/src")).expect("mkdir");
        let second = trail.refresh(root);
        assert!(
            second.contains(&PathBuf::from("crates/new")),
            "the new directory joins the set: {second:?}"
        );
        assert!(
            second.contains(&PathBuf::from("crates/new/src")),
            "and the subtree unfolds down its own path in the SAME scan: \
             {second:?}"
        );
    }

    #[test]
    fn a_trusted_device_readdirs_nothing_when_no_directory_changed() {
        // Steady state: O(known dirs) lstats, zero readdirs. Trust is granted
        // through the ledger seam (the fixture's real device is honest, but the
        // guard must not depend on winning a promotion race).
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src/inner")).expect("mkdir");

        let trail = WatchTrail::default();
        let dev = device_under(root);
        for _ in 0..PROMOTION_DEMONSTRATIONS {
            trail.record_canary_for_test(dev, true);
        }
        // Age every directory so no observed mtime ties the scan boundary —
        // otherwise the racy rule (correctly) re-verifies them.
        age(root);
        age(&root.join("src"));
        age(&root.join("src/inner"));

        trail.refresh(root);
        let after_seed = trail.readdir_count();
        trail.refresh(root);
        assert_eq!(
            trail.readdir_count(),
            after_seed,
            "an unchanged, trusted tree is pure lstats — no traversal"
        );
    }

    #[test]
    fn an_unclassified_device_readdirs_unconditionally() {
        // Degraded mode: more I/O, same truth. Nothing is trusted until it is
        // demonstrated, so a fresh (or swapped) device always readdirs.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir(root.join("src")).expect("mkdir");
        age(root);
        age(&root.join("src"));

        let trail = WatchTrail::default();
        assert_eq!(
            trail.verdict_of(device_under(root)),
            Verdict::Unclassified,
            "nothing is trusted before it is demonstrated"
        );
        trail.refresh(root);
        let after_seed = trail.readdir_count();
        trail.refresh(root);
        assert!(
            trail.readdir_count() > after_seed,
            "an unclassified device re-enumerates rather than trusting equality"
        );
    }

    #[test]
    fn a_racy_directory_is_reverified_even_when_trusted() {
        // The granularity race: a directory whose mtime ties the scan boundary
        // could have changed within the same tick, so equality proves nothing.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir(root.join("src")).expect("mkdir");

        let trail = WatchTrail::default();
        let dev = device_under(root);
        for _ in 0..PROMOTION_DEMONSTRATIONS {
            trail.record_canary_for_test(dev, true);
        }
        // Stamp both directories INTO the future: their mtimes necessarily tie
        // or exceed the next scan's boundary.
        let future = std::time::SystemTime::now() + std::time::Duration::from_hours(1);
        for dir in [root.to_path_buf(), root.join("src")] {
            filetime::set_file_mtime(&dir, filetime::FileTime::from_system_time(future))
                .expect("set dir mtime");
        }

        trail.refresh(root);
        let after_seed = trail.readdir_count();
        trail.refresh(root);
        assert!(
            trail.readdir_count() > after_seed,
            "a directory whose mtime ties the snapshot boundary is re-verified"
        );
    }

    #[test]
    fn one_failed_canary_demotes_immediately_and_promotion_starts_over() {
        // Asymmetric hysteresis: proof of a lie outweighs any streak.
        let trail = WatchTrail::default();
        let dev = 42;
        for _ in 0..PROMOTION_DEMONSTRATIONS {
            trail.record_canary_for_test(dev, true);
        }
        assert_eq!(trail.verdict_of(dev), Verdict::Trusted);

        trail.record_canary_for_test(dev, false);
        assert_eq!(
            trail.verdict_of(dev),
            Verdict::Degraded,
            "one demonstrated failure demotes immediately"
        );

        // A single positive must NOT restore trust — promotion needs fresh
        // demonstrations, counted from zero.
        trail.record_canary_for_test(dev, true);
        assert_eq!(
            trail.verdict_of(dev),
            Verdict::Degraded,
            "promotion requires fresh demonstrations, not one rebound"
        );
        for _ in 1..PROMOTION_DEMONSTRATIONS {
            trail.record_canary_for_test(dev, true);
        }
        assert_eq!(
            trail.verdict_of(dev),
            Verdict::Trusted,
            "and re-earns trust once they are supplied"
        );
    }

    #[test]
    fn devices_are_classified_independently() {
        // A submount inside the tree is separately classified — the mount
        // boundary needs no detection of its own, it falls out of st_dev.
        let trail = WatchTrail::default();
        trail.record_canary_for_test(1, false);
        for _ in 0..PROMOTION_DEMONSTRATIONS {
            trail.record_canary_for_test(2, true);
        }
        assert_eq!(trail.verdict_of(1), Verdict::Degraded);
        assert_eq!(trail.verdict_of(2), Verdict::Trusted);
    }

    #[test]
    fn an_entry_changing_write_promotes_the_device_it_lands_on() {
        // The canary end to end on a real (honest) filesystem: book a creation,
        // let it land, scan — the directory's mtime moved, so the device earns
        // a positive demonstration.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let trail = WatchTrail::default();
        trail.refresh(root);
        age(root);
        // Re-observe so the aged mtime is the recorded pre-write value.
        trail.refresh(root);

        let created = root.join("brand_new.txt");
        trail.note_entry_changing_write(&created);
        std::fs::write(&created, "x\n").expect("write");

        trail.refresh(root);
        let dev = device_under(root);
        let state = trail
            .trust
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .devices
            .get(&dev)
            .copied()
            .expect("the canary produced a verdict");
        assert!(
            state.positives >= 1 || state.verdict == Verdict::Trusted,
            "a landed creation whose directory mtime advanced is a positive \
             demonstration: {state:?}"
        );
    }

    #[test]
    fn a_write_that_never_lands_produces_no_verdict() {
        // A resolved write whose command failed is not evidence about the
        // filesystem — it must not demote anything.
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let trail = WatchTrail::default();
        trail.refresh(root);

        trail.note_entry_changing_write(&root.join("never_written.txt"));
        trail.refresh(root);

        assert_eq!(
            trail.verdict_of(device_under(root)),
            Verdict::Unclassified,
            "a phantom write yields no verdict, in either direction"
        );
    }

    #[test]
    fn forget_root_drops_the_trail() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        std::fs::create_dir(root.join("src")).expect("mkdir");
        let trail = WatchTrail::default();
        trail.refresh(root);
        trail.forget_root(root);
        assert!(
            trail
                .roots
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_empty(),
            "a retired root leaves no trail behind"
        );
    }
}
