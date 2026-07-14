// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Idle detection: waits for the server process tree to go quiet.
//!
//! [`IdleDetector`] is a pure state machine that tracks baseline activity,
//! per-child gates, and quiet detection. Two constructors define the mode:
//!
//! - [`IdleDetector::after_activity`] — post-stimulus: requires observing
//!   activity (any nonzero delta) before accepting silence as idle.
//! - [`IdleDetector::unconditional`] — pre-stimulus: accepts silence
//!   immediately (no activity requirement).
//!
//! The production [`await_idle`] function wraps the polling loop, handling
//! budget, lifecycle, root death, and cancellation.
//!
//! The profiling [`profile_loop`] runs the sampling loop continuously and
//! yields per-process samples to a caller-provided [`ProfileSink`].

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use catenary_proc::{ProcessState, SCHEDULER_STATE_OBSERVABLE};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use super::server::LspServer;
use super::state::ServerLifecycle;
use crate::source::Source;

// ── Constants ────────────────────────────────────────────────────────

/// Polling interval for tree walks (validated by profiling).
///
/// Shared with the diagnostics retrieval evidence bar
/// (`await_publish_evidence`) so its quiet-sample accounting runs at the
/// same cadence as the settle loop it extends.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Poll-sample count after which an unsettled wait is *evidence-backed long*
/// and earns a heartbeat note (misc 160 leg 3 / bug 78 + 79 rider).
///
/// A **work count**, not a wall-clock bound: it counts poll iterations of the
/// settle loop, and crossing it adds **no** ceiling — settle stays deliberately
/// unbounded (bug 28/55). Its sole effect is to emit an `info!` note to the
/// firehose so a caller can tell a still-working server from a wedged one from
/// outside. A declared protocol constant (contention-doctrine exempt). At the
/// 50 ms [`POLL_INTERVAL`] this is a first note near ~15 s of continuous
/// waiting, then one per interval thereafter.
const HEARTBEAT_SAMPLE_INTERVAL: u32 = 300;

// ── IdleDetector ─────────────────────────────────────────────────────

/// Outcome of the idle detection operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleResult {
    /// Server settled — all processes quiet.
    Settled,
    /// Root process died.
    RootDied,
}

/// Stateful idle detector for server process trees.
///
/// Pure state machine: given a [`catenary_proc::TreeSnapshot`], determines
/// whether the server is idle. Does not own the polling loop — the caller
/// polls at their own cadence and calls [`IdleDetector::check`] with each
/// snapshot.
pub struct IdleDetector {
    /// Whether activity has been observed since construction.
    saw_activity: bool,
    /// Whether the first snapshot has been seen (initial PID population).
    seen_first: bool,
    /// Cumulative tick baseline for pre-stimulus comparison.
    /// When `Some`, phase 1 also checks `snapshot.cumulative_ticks > baseline`
    /// to detect sub-delta-resolution activity.
    baseline_ticks: Option<u64>,
    /// PIDs that have shown nonzero deltas (per-child gates).
    active_pids: HashSet<u32>,
    /// All PIDs seen so far (for new-PID detection).
    known_pids: HashSet<u32>,
}

impl IdleDetector {
    /// Post-stimulus mode: requires observing activity before accepting idle.
    ///
    /// `baseline_ticks` is the [`catenary_proc::TreeSnapshot::cumulative_ticks`]
    /// from a sample taken immediately before the stimulus. If the server
    /// burns even 1 page fault processing the stimulus, cumulative ticks
    /// advance and the work gate fires on the first poll — no timeout needed.
    ///
    /// Two internal phases:
    /// 1. Wait for cumulative ticks to advance from baseline, or any nonzero
    ///    delta. Either proves the server was scheduled.
    /// 2. Wait for the tree to go quiet: zero deltas, no new PIDs, no live
    ///    process runnable or blocked (scheduler-state ground truth), and
    ///    per-child gates satisfied.
    #[must_use]
    pub fn after_activity(baseline_ticks: u64) -> Self {
        Self {
            saw_activity: false,
            seen_first: false,
            baseline_ticks: Some(baseline_ticks),
            active_pids: HashSet::new(),
            known_pids: HashSet::new(),
        }
    }

    /// Pre-stimulus mode: no activity requirement.
    ///
    /// Compares consecutive samples for zero deltas immediately.
    /// Used to ensure the server is quiet before sending a stimulus.
    #[must_use]
    pub fn unconditional() -> Self {
        Self {
            saw_activity: true,
            seen_first: false,
            baseline_ticks: None,
            active_pids: HashSet::new(),
            known_pids: HashSet::new(),
        }
    }

    /// Checks whether the server is idle given the current tree snapshot.
    ///
    /// Idle requires zero CPU/page-fault deltas across the whole tree, no
    /// newly-appeared PIDs, per-child activity gates satisfied, and — where
    /// scheduler state is observable — every live process in a sleep-class
    /// state. A live process that is runnable or blocked is pending work and
    /// blocks idle regardless of deltas, so a child starved of CPU under host
    /// pressure can no longer read as idle across an all-zero window (bug 55).
    ///
    /// Returns `true` when idle is detected.
    #[allow(
        clippy::similar_names,
        reason = "delta_utime/delta_stime are standard counter names"
    )]
    pub fn check(&mut self, snapshot: &catenary_proc::TreeSnapshot) -> bool {
        let first = !self.seen_first;
        self.seen_first = true;

        let mut any_nonzero = false;
        let mut new_pids = false;
        let mut any_pending = false;

        for ts in &snapshot.samples {
            let is_active = ts.delta_pfc > 0 || ts.delta_utime > 0 || ts.delta_stime > 0;

            if is_active {
                any_nonzero = true;
                self.active_pids.insert(ts.pid);
            }

            // Per-child cumulative-ticks admission (bug 107). A child that
            // spawns after the first snapshot, does all its work inside one
            // 50ms poll window, then sleeps forever never shows a sampled
            // delta — `is_active` is false on every poll — so the per-child
            // gate below could never admit it and settle spun "quiet but not
            // settled" unbounded. Its cumulative counter is nonzero, though:
            // proof it has run. Admit on that evidence, the per-child
            // counterpart of phase 1's `cumulative_advanced`. Evidence-based,
            // not a timeout: a genuinely new PID with zero cumulative work
            // (spawned, never scheduled) is not admitted here and still blocks,
            // and a runnable/blocked child is still caught by `any_pending`
            // below regardless of this admission (bugs 28/55 preserved).
            if ts.cumulative_ticks > 0 {
                self.active_pids.insert(ts.pid);
            }

            // Scheduler-state ground truth (bug 55): a live process that is
            // runnable or blocked has pending work by definition, independent
            // of the sampled deltas. Under CPU pressure a starved child sits
            // in the run queue unscheduled for a whole 50ms window — zero
            // utime/stime/pfc while ticks of work remain — and the delta-only
            // predicate read that as idle, settling early and reporting
            // `[clean]`. Blocks idle regardless of deltas; pressure-independent.
            if is_pending_work(ts.state) {
                any_pending = true;
            }

            if self.known_pids.insert(ts.pid) {
                if first {
                    // Initial population: gate-satisfied by default.
                    // These PIDs were present before the stimulus.
                    self.active_pids.insert(ts.pid);
                } else {
                    // Genuinely new PID — must show activity before
                    // it can contribute to idle detection.
                    new_pids = true;
                }
            }
        }

        // Phase 1: wait for activity
        if !self.saw_activity {
            // Check cumulative ticks against pre-stimulus baseline.
            // Catches sub-delta-resolution activity (e.g., fast servers
            // that process in <10ms but still cause context switches).
            let cumulative_advanced = self
                .baseline_ticks
                .is_some_and(|base| snapshot.cumulative_ticks > base);

            if any_nonzero || cumulative_advanced {
                self.saw_activity = true;
                debug!("idle_detector: activity observed");
            } else {
                return false;
            }
        }

        // Phase 2: quiet detection. Idle requires zero deltas, no new PIDs,
        // and no live process runnable or blocked (scheduler-state ground
        // truth) — every live process must be in a sleep-class state.
        if any_nonzero || new_pids || any_pending {
            return false;
        }

        // Per-child gates: every live process must have shown activity
        snapshot
            .samples
            .iter()
            .all(|ts| ts.state == ProcessState::Dead || self.active_pids.contains(&ts.pid))
    }
}

/// Whether a process sample represents pending work that must block idle
/// regardless of the sampled CPU deltas.
///
/// On platforms where scheduler state is observable
/// ([`catenary_proc::SCHEDULER_STATE_OBSERVABLE`]), a live process that is
/// [`ProcessState::Running`] (on a core or waiting in the run queue — a
/// starved-but-runnable process is still pending work) or
/// [`ProcessState::Blocked`] (uninterruptible kernel I/O in flight) is not
/// idle even across an all-zero sampling window. On platforms without
/// observable scheduler state (Windows reports every live process as
/// `Running` as a liveness placeholder), this is always `false` and idle
/// detection falls back to CPU deltas alone.
const fn is_pending_work(state: ProcessState) -> bool {
    SCHEDULER_STATE_OBSERVABLE && matches!(state, ProcessState::Running | ProcessState::Blocked)
}

// ── await_idle ───────────────────────────────────────────────────────

/// Waits for the server to go idle using the provided detector.
///
/// Runs a 50ms polling loop, skipping the tree walk while the server is
/// `Busy(n)` (an open `$/progress` bracket — explained work), and delegating
/// idle detection to [`IdleDetector::check`].
///
/// There is deliberately **no CPU-time cap**: the detector watches the whole
/// subtree, so a flycheck burning CPU in `cargo`/`rustc` children keeps the
/// settle open until that work finishes — which is the point. The only bounds
/// are a quiet tree (settled), root death, and the cancel token. The caller
/// owns liveness: the diagnostics batch runs under cancel-on-disconnect, so a
/// genuinely-wedged server is torn down when the client gives up (bug 24).
/// A tree-summed CPU budget here used to bail on the legitimate parallel
/// flycheck and report `[clean]` (bug 28).
///
/// Returns when the server is idle, the root process dies, or the cancel
/// token fires.
///
/// `server_name` names the server in the settle heartbeat
/// ([`HEARTBEAT_SAMPLE_INTERVAL`]) — an `info!` note emitted while the wait is
/// evidence-backed long, so a still-working server is distinguishable from a
/// wedged one from outside without touching any output contract or adding any
/// bound (misc 160 leg 3).
pub async fn await_idle(
    server: &Arc<LspServer>,
    mut detector: IdleDetector,
    cancel: CancellationToken,
    server_name: &str,
) -> SettleResult {
    // Poll-sample counter feeding the heartbeat. Counts iterations of this
    // loop, never elapsed wall-clock time — a work count (contention doctrine).
    let mut samples: u32 = 0;
    loop {
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL) => {}
            () = cancel.cancelled() => { return SettleResult::Settled; }
        }

        samples = samples.saturating_add(1);

        let lifecycle = server.lifecycle();

        // Terminal states
        if lifecycle.is_terminal() {
            return SettleResult::RootDied;
        }

        // During Busy: activity is implicit (an open `$/progress` bracket),
        // skip tree walking. The server is provably working, so the heartbeat
        // reports it as such.
        if matches!(lifecycle, ServerLifecycle::Busy(_)) {
            heartbeat(server_name, samples, true);
            continue;
        }

        // Sample the process tree via spawn_blocking (/proc reads are sync)
        let server_clone = Arc::clone(server);
        let Ok(Some(snapshot)) =
            tokio::task::spawn_blocking(move || server_clone.sample_tree()).await
        else {
            server.set_lifecycle(ServerLifecycle::Dead);
            return SettleResult::RootDied;
        };

        // Root death check
        if let Some(result) = check_root_death(server, &snapshot) {
            return result;
        }

        // Idle check — the whole subtree must be quiet (children/grandchildren
        // included), so a busy flycheck child holds the settle open.
        if detector.check(&snapshot) {
            debug!("idle_detector: server idle");
            return SettleResult::Settled;
        }

        // Not settled this poll. Once the wait is evidence-backed long, note it
        // — carrying whether the tree still shows CPU/scheduler activity
        // (working) or is quiet-but-gated (which reads as wedged from outside).
        heartbeat(server_name, samples, tree_working(&snapshot));
    }
}

/// Whether a settle snapshot still shows the server doing work: any nonzero CPU
/// or page-fault delta, or — where scheduler state is observable — any live
/// process runnable or blocked (the same pending-work ground truth
/// [`IdleDetector::check`] gates idle on). A `true` here means "still working";
/// a `false` across a long wait is the wedged signature.
///
/// Also consumed by the diagnostics retrieval evidence bar: activity extends
/// its publish wait (work-based doctrine), only quiet samples consume its
/// dead-air budget.
pub(crate) fn tree_working(snapshot: &catenary_proc::TreeSnapshot) -> bool {
    snapshot.samples.iter().any(|ts| {
        ts.delta_pfc > 0 || ts.delta_utime > 0 || ts.delta_stime > 0 || is_pending_work(ts.state)
    })
}

/// Whether a poll-sample count has crossed a [`HEARTBEAT_SAMPLE_INTERVAL`]
/// boundary and is thus due for a heartbeat note. A work count — evaluated on
/// the sample counter, never elapsed time.
const fn heartbeat_due(samples: u32) -> bool {
    samples != 0 && samples.is_multiple_of(HEARTBEAT_SAMPLE_INTERVAL)
}

/// Emits the settle heartbeat when the wait crosses a [`HEARTBEAT_SAMPLE_INTERVAL`]
/// boundary.
///
/// `info!` only — a long-but-working settle is not an actionable interrupt
/// (`error!` fires a desktop notification, `warn!` raises a TUI finding; neither
/// is warranted). The note lands in the firehose, the "from outside"
/// observability surface (`catenary query`), carrying the poll-sample count and
/// the `working` flag so working and wedged are distinguishable. No bound is
/// added — the caller keeps waiting.
fn heartbeat(server_name: &str, samples: u32, working: bool) {
    if !heartbeat_due(samples) {
        return;
    }
    info!(
        source = Source::LspLifecycle.as_str(),
        server = server_name,
        samples,
        working,
        "settling: {server_name} {}, {samples} samples",
        if working {
            "still working"
        } else {
            "quiet but not settled"
        },
    );
}

/// Pure root-death detection logic — determines whether the root process
/// is gone based on a PID and snapshot alone.
fn root_state(
    root_pid: Option<u32>,
    snapshot: &catenary_proc::TreeSnapshot,
) -> Option<SettleResult> {
    if snapshot.process_count == 0 || snapshot.samples.is_empty() {
        return Some(SettleResult::RootDied);
    }

    let Some(root_pid) = root_pid else {
        return Some(SettleResult::RootDied);
    };

    match snapshot.samples.iter().find(|s| s.pid == root_pid) {
        Some(root) if root.state == ProcessState::Dead => Some(SettleResult::RootDied),
        None => Some(SettleResult::RootDied),
        _ => None,
    }
}

/// Checks whether the root process has died and transitions lifecycle.
fn check_root_death(
    server: &LspServer,
    snapshot: &catenary_proc::TreeSnapshot,
) -> Option<SettleResult> {
    let result = root_state(server.pid(), snapshot)?;
    debug!("idle_detector: root process gone");
    server.set_lifecycle(ServerLifecycle::Dead);
    Some(result)
}

// ── Profiling loop ───────────────────────────────────────────────────

/// A single sample from one process in the tree.
pub struct ProfileSample {
    /// When the sample was taken.
    pub timestamp: std::time::Instant,
    /// Server name (e.g. `"rust-analyzer"`).
    pub server: String,
    /// Process ID.
    pub pid: u32,
    /// Parent process ID.
    pub ppid: u32,
    /// Page fault count delta since last sample.
    pub delta_pfc: u64,
    /// User CPU time delta since last sample (centiseconds).
    pub delta_utime: u64,
    /// System CPU time delta since last sample (centiseconds).
    pub delta_stime: u64,
    /// Count of in-flight progress tokens at sample time.
    pub in_progress_count: u32,
    /// Total processes in the tree at this sample.
    pub process_count: usize,
}

/// Receives samples from the profiling loop.
///
/// Sync and non-async — recording to a database or vec is a blocking
/// operation, and the profiling loop controls its own async timing.
pub trait ProfileSink: Send {
    /// Called for each per-process sample. Return `false` to stop the loop.
    fn record(&mut self, sample: &ProfileSample) -> bool;
}

/// Run the profiling sampling loop.
///
/// Polls `tree_monitor` every `interval`, reads `in_progress_count` from
/// `server`, and calls `sink.record()` for each per-process sample.
/// Runs until `sink.record()` returns `false` or the `cancel` token fires.
///
/// This is the profiling variant — it yields every sample to the sink for
/// recording. The production [`await_idle`] function uses the tree monitor on
/// [`LspServer`] and makes idle decisions internally.
#[allow(
    clippy::similar_names,
    reason = "delta_utime/delta_stime are standard counter names"
)]
pub async fn profile_loop(
    tree_monitor: &mut catenary_proc::TreeMonitor,
    server: &LspServer,
    server_name: &str,
    interval: Duration,
    sink: &mut dyn ProfileSink,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = cancel.cancelled() => { return; }
        }

        let snapshot = tree_monitor.sample();
        let in_progress_count = server.in_progress_count();
        let timestamp = std::time::Instant::now();

        for ts in &snapshot.samples {
            let sample = ProfileSample {
                timestamp,
                server: server_name.to_string(),
                pid: ts.pid,
                ppid: ts.ppid,
                delta_pfc: ts.delta_pfc,
                delta_utime: ts.delta_utime,
                delta_stime: ts.delta_stime,
                in_progress_count,
                process_count: snapshot.process_count,
            };

            if !sink.record(&sample) {
                return;
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use catenary_proc::{TreeSample, TreeSnapshot};

    fn test_server() -> LspServer {
        LspServer::new("test".to_string(), "test-server".to_string(), None)
    }

    // ── IdleDetector unit tests ─────────────────────────────────────

    fn make_snapshot(samples: Vec<TreeSample>) -> TreeSnapshot {
        let process_count = samples.len();
        TreeSnapshot {
            samples,
            process_count,
            cumulative_ticks: 0,
        }
    }

    fn active_sample(pid: u32) -> TreeSample {
        TreeSample {
            pid,
            ppid: 1,
            delta_utime: 5,
            delta_stime: 2,
            delta_pfc: 10,
            // An actively-working process has run: nonzero cumulative.
            cumulative_ticks: 17,
            state: ProcessState::Running,
        }
    }

    /// A quiescent sample: zero deltas, zero cumulative, and a sleep-class
    /// state. Quiet means sleep-class — a zero-delta `Running`/`Blocked`
    /// process is pending work, not idle (bug 55), so quiet fixtures must be
    /// `Sleeping`. Zero cumulative means "no proven work yet" — a PID that
    /// only ever appears via `quiet_sample` has shown no evidence it ran, so
    /// the per-child gate (delta or cumulative) is not satisfied for it.
    fn quiet_sample(pid: u32) -> TreeSample {
        TreeSample {
            pid,
            ppid: 1,
            delta_utime: 0,
            delta_stime: 0,
            delta_pfc: 0,
            cumulative_ticks: 0,
            state: ProcessState::Sleeping,
        }
    }

    /// A zero-delta, zero-cumulative sample in an arbitrary state — for the
    /// scheduler-state regression tests that assert Running/Blocked block idle
    /// while Sleeping settles. Zero cumulative keeps those tests exercising the
    /// scheduler-state predicate rather than the cumulative admission.
    fn zero_delta_sample(pid: u32, state: ProcessState) -> TreeSample {
        TreeSample {
            pid,
            ppid: 1,
            delta_utime: 0,
            delta_stime: 0,
            delta_pfc: 0,
            cumulative_ticks: 0,
            state,
        }
    }

    #[test]
    fn after_activity_requires_nonzero_before_idle() {
        let mut detector = IdleDetector::after_activity(0);
        // First poll: all zeros — not idle yet (no activity seen)
        let snap = make_snapshot(vec![quiet_sample(100)]);
        assert!(!detector.check(&snap));

        // Second poll: still zeros — still not idle
        assert!(!detector.check(&snap));
    }

    #[test]
    fn after_activity_cumulative_baseline_detects_fast_server() {
        // Baseline: cumulative_ticks = 100
        let mut detector = IdleDetector::after_activity(100);

        // First poll: deltas are zero (sub-resolution processing),
        // but cumulative advanced from 100 → 101 (1 context switch).
        let mut snap = make_snapshot(vec![quiet_sample(100)]);
        snap.cumulative_ticks = 101;
        // Activity detected via cumulative comparison — and the snapshot
        // IS quiet, so idle is detected on the same poll.
        assert!(detector.check(&snap));
    }

    #[test]
    fn after_activity_cumulative_no_advance_stays_waiting() {
        // Baseline: cumulative_ticks = 100
        let mut detector = IdleDetector::after_activity(100);

        // Cumulative unchanged, deltas zero — activity not yet observed
        let mut snap = make_snapshot(vec![quiet_sample(100)]);
        snap.cumulative_ticks = 100;
        assert!(!detector.check(&snap));
    }

    #[test]
    fn after_activity_detects_idle_after_work() {
        let mut detector = IdleDetector::after_activity(0);

        // Activity observed
        let active = make_snapshot(vec![active_sample(100)]);
        assert!(!detector.check(&active));

        // Now quiet — idle
        let quiet = make_snapshot(vec![quiet_sample(100)]);
        assert!(detector.check(&quiet));
    }

    #[test]
    fn unconditional_detects_idle_immediately_on_quiet() {
        let mut detector = IdleDetector::unconditional();

        // First poll: all zeros — idle immediately (no activity required)
        let snap = make_snapshot(vec![quiet_sample(100)]);
        assert!(detector.check(&snap));
    }

    #[test]
    fn unconditional_waits_through_activity() {
        let mut detector = IdleDetector::unconditional();

        // Active — not idle
        let active = make_snapshot(vec![active_sample(100)]);
        assert!(!detector.check(&active));

        // Quiet — idle
        let quiet = make_snapshot(vec![quiet_sample(100)]);
        assert!(detector.check(&quiet));
    }

    #[test]
    fn per_child_gate_blocks_idle_for_unseen_pid() {
        let mut detector = IdleDetector::after_activity(0);

        // PID 100 shows activity
        let snap1 = make_snapshot(vec![active_sample(100)]);
        assert!(!detector.check(&snap1));

        // PID 100 quiet, but new PID 200 appears — not idle (new PID)
        let snap2 = make_snapshot(vec![quiet_sample(100), quiet_sample(200)]);
        assert!(!detector.check(&snap2));

        // Both quiet, but PID 200 never showed activity — not idle (gate)
        let snap3 = make_snapshot(vec![quiet_sample(100), quiet_sample(200)]);
        assert!(!detector.check(&snap3));

        // PID 200 shows activity
        let snap4 = make_snapshot(vec![quiet_sample(100), active_sample(200)]);
        assert!(!detector.check(&snap4));

        // Both quiet, both gates satisfied — idle
        let snap5 = make_snapshot(vec![quiet_sample(100), quiet_sample(200)]);
        assert!(detector.check(&snap5));
    }

    // ── Per-child cumulative-ticks admission (bug 107) ───────────────

    /// A fast-quiet child: zero deltas but a nonzero cumulative counter and a
    /// sleep-class state. It did all its work inside one poll window (so no
    /// window ever caught a delta) and now sleeps forever, but its cumulative
    /// counter proves it ran. This is the shape that spun settle unbounded.
    fn fast_quiet_sample(pid: u32) -> TreeSample {
        TreeSample {
            pid,
            ppid: 1,
            delta_utime: 0,
            delta_stime: 0,
            delta_pfc: 0,
            cumulative_ticks: 42,
            state: ProcessState::Sleeping,
        }
    }

    #[test]
    fn fast_quiet_child_settles_on_cumulative_evidence() {
        // Bug 107: a child spawns after the first snapshot, does all its work
        // inside one 50ms poll window, then sleeps forever. No poll ever catches
        // a nonzero delta for it — so before the fix the per-child gate could
        // never admit it and settle spun "quiet but not settled" unbounded. Its
        // cumulative counter is nonzero, proving it ran: admit on that evidence.
        let mut detector = IdleDetector::after_activity(0);

        // PID 100 (the root) shows activity — phase 1 satisfied.
        assert!(!detector.check(&make_snapshot(vec![active_sample(100)])));

        // Fast-quiet child 200 appears: new PID this poll → blocked (new_pids),
        // but its nonzero cumulative admits it to the per-child gate now.
        let appears = make_snapshot(vec![quiet_sample(100), fast_quiet_sample(200)]);
        assert!(
            !detector.check(&appears),
            "a genuinely new PID blocks idle for the poll it first appears"
        );

        // Next poll: 200 is no longer new, still zero deltas, still Sleeping,
        // cumulative still nonzero. Root 100 was admitted by its delta; 200 is
        // admitted by cumulative evidence — so idle is now reachable.
        let settled = make_snapshot(vec![quiet_sample(100), fast_quiet_sample(200)]);
        assert!(
            detector.check(&settled),
            "a fast-quiet child that never showed a delta but has run \
             (nonzero cumulative) must not block idle forever"
        );
    }

    #[test]
    fn starved_child_stays_blocked_despite_cumulative() {
        // Bug 55 must survive the cumulative admission: a live runnable child
        // has pending work regardless of deltas OR cumulative. Even with a
        // nonzero cumulative counter, a Running (starved) child blocks idle via
        // any_pending wherever scheduler state is observable — cumulative
        // admission satisfies the per-child gate, but any_pending gates first.
        let mut detector = IdleDetector::after_activity(0);

        assert!(!detector.check(&make_snapshot(vec![active_sample(100)])));

        // PID 100 now zero-delta but Running, with a nonzero cumulative.
        let starved = make_snapshot(vec![TreeSample {
            pid: 100,
            ppid: 1,
            delta_utime: 0,
            delta_stime: 0,
            delta_pfc: 0,
            cumulative_ticks: 99,
            state: ProcessState::Running,
        }]);
        assert_eq!(
            detector.check(&starved),
            !SCHEDULER_STATE_OBSERVABLE,
            "a starved-but-runnable child blocks idle where scheduler state is \
             observable, even with a nonzero cumulative counter (bug 55)"
        );
    }

    #[test]
    fn zero_cumulative_new_pid_stays_blocked() {
        // A genuinely new PID with zero cumulative work (spawned, never
        // scheduled) has shown no evidence it ran — neither a delta nor a
        // nonzero cumulative — so the per-child gate must keep blocking idle
        // until it shows evidence either way. The cumulative admission is
        // strictly evidence-based, not a timeout: no evidence, no admission.
        let mut detector = IdleDetector::after_activity(0);

        assert!(!detector.check(&make_snapshot(vec![active_sample(100)])));

        // New PID 200 with zero cumulative appears → blocked (new_pids).
        let appears = make_snapshot(vec![quiet_sample(100), quiet_sample(200)]);
        assert!(!detector.check(&appears));

        // Next poll: 200 no longer new, zero deltas, zero cumulative, Sleeping.
        // No evidence it ever ran → per-child gate keeps blocking, unbounded
        // until it either shows work (admit) or dies (bypass).
        let still = make_snapshot(vec![quiet_sample(100), quiet_sample(200)]);
        assert!(
            !detector.check(&still),
            "a new PID with zero cumulative work has shown no evidence and must \
             keep blocking idle — cumulative admission is evidence-based only"
        );
    }

    #[test]
    fn dead_process_bypasses_gate() {
        let mut detector = IdleDetector::after_activity(0);

        // PID 100 active
        let snap1 = make_snapshot(vec![active_sample(100)]);
        assert!(!detector.check(&snap1));

        // PID 200 appears dead — gate bypassed
        let snap2 = make_snapshot(vec![
            quiet_sample(100),
            TreeSample {
                pid: 200,
                ppid: 1,
                delta_utime: 0,
                delta_stime: 0,
                delta_pfc: 0,
                cumulative_ticks: 0,
                state: ProcessState::Dead,
            },
        ]);
        // New PID in this poll → not idle
        assert!(!detector.check(&snap2));

        // Next poll: same set, all quiet, 200 is dead → idle
        let snap3 = make_snapshot(vec![
            quiet_sample(100),
            TreeSample {
                pid: 200,
                ppid: 1,
                delta_utime: 0,
                delta_stime: 0,
                delta_pfc: 0,
                cumulative_ticks: 0,
                state: ProcessState::Dead,
            },
        ]);
        assert!(detector.check(&snap3));
    }

    #[test]
    fn pfc_only_activity_detects_work() {
        let mut detector = IdleDetector::after_activity(0);

        // Page faults only (no CPU time) — should still count as activity
        let pfc_only = make_snapshot(vec![TreeSample {
            pid: 100,
            ppid: 1,
            delta_utime: 0,
            delta_stime: 0,
            delta_pfc: 5,
            cumulative_ticks: 5,
            state: ProcessState::Running,
        }]);
        assert!(
            !detector.check(&pfc_only),
            "pfc-only should register activity"
        );

        // Now quiet — idle
        let quiet = make_snapshot(vec![quiet_sample(100)]);
        assert!(detector.check(&quiet));
    }

    // ── Scheduler-state predicate (bug 55) ──────────────────────────

    #[test]
    fn running_zero_deltas_blocks_idle() {
        // A runnable process with zero observed deltas is starved — sitting
        // in the run queue waiting for a core while work remains pending —
        // not idle. Scheduler state `Running` blocks idle regardless of
        // deltas wherever scheduler state is observable.
        let mut detector = IdleDetector::after_activity(0);

        // Observe activity first so phase 1 is satisfied.
        assert!(!detector.check(&make_snapshot(vec![active_sample(100)])));

        // PID 100 now shows zero deltas but is still Running (starved under
        // CPU pressure). Must NOT settle on observable-scheduler platforms.
        let starved = make_snapshot(vec![zero_delta_sample(100, ProcessState::Running)]);
        assert_eq!(
            detector.check(&starved),
            !SCHEDULER_STATE_OBSERVABLE,
            "Running + zero deltas is a starved-but-runnable process; it must \
             block idle where scheduler state is observable"
        );
    }

    #[test]
    fn blocked_zero_deltas_blocks_idle() {
        // Uninterruptible sleep (`D`) has kernel I/O in flight — pending work
        // — even with zero CPU deltas.
        let mut detector = IdleDetector::after_activity(0);

        assert!(!detector.check(&make_snapshot(vec![active_sample(100)])));

        let blocked = make_snapshot(vec![zero_delta_sample(100, ProcessState::Blocked)]);
        assert_eq!(
            detector.check(&blocked),
            !SCHEDULER_STATE_OBSERVABLE,
            "Blocked (uninterruptible I/O) + zero deltas is pending work; it \
             must block idle where scheduler state is observable"
        );
    }

    #[test]
    fn sleeping_zero_deltas_settles_after_activity() {
        // Sleep-class is the only state that permits idle. After observing
        // activity, a Sleeping process with zero deltas settles on every
        // platform.
        let mut detector = IdleDetector::after_activity(0);

        assert!(!detector.check(&make_snapshot(vec![active_sample(100)])));

        let sleeping = make_snapshot(vec![zero_delta_sample(100, ProcessState::Sleeping)]);
        assert!(
            detector.check(&sleeping),
            "Sleeping + zero deltas after activity is genuinely idle"
        );
    }

    // ── settle heartbeat (misc 160 leg 3) ───────────────────────────

    #[test]
    fn heartbeat_due_only_on_sample_boundaries() {
        // The heartbeat is a work count over poll samples — it fires only when
        // the counter crosses a HEARTBEAT_SAMPLE_INTERVAL multiple, never before
        // the first interval and never off-boundary. No wall-clock is consulted.
        assert!(!heartbeat_due(0), "sample 0 is never due");
        assert!(!heartbeat_due(1), "before the first interval: not due");
        assert!(
            !heartbeat_due(HEARTBEAT_SAMPLE_INTERVAL - 1),
            "one short of the boundary: not due"
        );
        assert!(
            heartbeat_due(HEARTBEAT_SAMPLE_INTERVAL),
            "first boundary: due"
        );
        assert!(
            !heartbeat_due(HEARTBEAT_SAMPLE_INTERVAL + 1),
            "just past the boundary: not due"
        );
        assert!(
            heartbeat_due(HEARTBEAT_SAMPLE_INTERVAL * 3),
            "every interval thereafter: due"
        );
    }

    #[test]
    fn tree_working_distinguishes_working_from_quiet() {
        // A snapshot with any CPU/page-fault delta is "still working".
        assert!(
            tree_working(&make_snapshot(vec![active_sample(100)])),
            "nonzero deltas read as working"
        );
        // An all-quiet (Sleeping, zero-delta) tree is not working — the wedged
        // signature when it persists across a long wait.
        assert!(
            !tree_working(&make_snapshot(vec![quiet_sample(100)])),
            "a Sleeping, zero-delta tree is not working"
        );
        // Where scheduler state is observable, a zero-delta Running process is
        // pending work (starved-but-runnable) and reads as working; where it is
        // not observable, deltas alone decide.
        assert_eq!(
            tree_working(&make_snapshot(vec![zero_delta_sample(
                100,
                ProcessState::Running
            )])),
            SCHEDULER_STATE_OBSERVABLE,
            "a zero-delta Running process is pending work where scheduler state is observable"
        );
    }

    // ── root_state unit tests ─────────────────────────────────────────

    #[test]
    fn root_state_empty_snapshot() {
        let snapshot = TreeSnapshot {
            samples: Vec::new(),
            process_count: 0,
            cumulative_ticks: 0,
        };
        assert_eq!(root_state(Some(1), &snapshot), Some(SettleResult::RootDied));
    }

    #[test]
    fn root_state_no_pid() {
        let snapshot = make_snapshot(vec![active_sample(100)]);
        assert_eq!(root_state(None, &snapshot), Some(SettleResult::RootDied));
    }

    #[test]
    fn root_state_healthy_root() {
        let snapshot = make_snapshot(vec![quiet_sample(100)]);
        assert_eq!(root_state(Some(100), &snapshot), None);
    }

    #[test]
    fn root_state_dead_root() {
        let snapshot = make_snapshot(vec![TreeSample {
            pid: 100,
            ppid: 1,
            delta_utime: 0,
            delta_stime: 0,
            delta_pfc: 0,
            cumulative_ticks: 0,
            state: ProcessState::Dead,
        }]);
        assert_eq!(
            root_state(Some(100), &snapshot),
            Some(SettleResult::RootDied)
        );
    }

    #[test]
    fn root_state_pid_missing_from_snapshot() {
        let snapshot = make_snapshot(vec![quiet_sample(200)]);
        assert_eq!(
            root_state(Some(100), &snapshot),
            Some(SettleResult::RootDied)
        );
    }

    #[test]
    fn root_state_empty_samples_nonzero_count() {
        // Defensive: process_count disagrees with samples length
        let snapshot = TreeSnapshot {
            samples: Vec::new(),
            process_count: 5,
            cumulative_ticks: 0,
        };
        assert_eq!(root_state(Some(1), &snapshot), Some(SettleResult::RootDied));
    }

    #[test]
    fn root_state_zero_count_with_samples() {
        // Defensive: process_count is 0 but samples exist
        let snapshot = TreeSnapshot {
            samples: vec![quiet_sample(100)],
            process_count: 0,
            cumulative_ticks: 0,
        };
        assert_eq!(
            root_state(Some(100), &snapshot),
            Some(SettleResult::RootDied)
        );
    }

    // ── await_idle integration tests (no real server) ───────────────

    #[tokio::test]
    async fn await_idle_returns_root_died_for_terminal_state() {
        let server = Arc::new(test_server());
        server.set_lifecycle(ServerLifecycle::Dead);
        let cancel = CancellationToken::new();
        let detector = IdleDetector::unconditional();
        let result = await_idle(&server, detector, cancel, "test-server").await;
        assert_eq!(result, SettleResult::RootDied);
    }

    #[tokio::test]
    async fn await_idle_returns_root_died_without_tree_monitor() {
        let server = Arc::new(test_server());
        server.set_lifecycle(ServerLifecycle::Healthy);
        let cancel = CancellationToken::new();
        let detector = IdleDetector::unconditional();
        let result = await_idle(&server, detector, cancel, "test-server").await;
        assert_eq!(result, SettleResult::RootDied);
    }

    #[test]
    fn check_root_death_empty_snapshot() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        let snapshot = TreeSnapshot {
            samples: Vec::new(),
            process_count: 0,
            cumulative_ticks: 0,
        };
        let result = check_root_death(&server, &snapshot);
        assert_eq!(result, Some(SettleResult::RootDied));
        assert_eq!(server.lifecycle(), ServerLifecycle::Dead);
    }

    #[test]
    fn check_root_death_zombie_root() {
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        // Server has no connection, so pid() returns None → RootDied
        let snapshot = TreeSnapshot {
            samples: vec![TreeSample {
                pid: 1234,
                ppid: 1,
                delta_utime: 0,
                delta_stime: 0,
                delta_pfc: 0,
                cumulative_ticks: 0,
                state: ProcessState::Dead,
            }],
            process_count: 1,
            cumulative_ticks: 0,
        };
        let result = check_root_death(&server, &snapshot);
        // No PID (no connection) → RootDied
        assert_eq!(result, Some(SettleResult::RootDied));
    }

    #[test]
    fn check_root_death_healthy_root_returns_none() {
        // Without a connection, pid() returns None, so this always returns RootDied.
        // Full settle integration requires a real server process.
        let server = test_server();
        server.set_lifecycle(ServerLifecycle::Healthy);
        let snapshot = TreeSnapshot {
            samples: vec![TreeSample {
                pid: 1234,
                ppid: 1,
                delta_utime: 5,
                delta_stime: 2,
                delta_pfc: 100,
                cumulative_ticks: 107,
                state: ProcessState::Running,
            }],
            process_count: 1,
            cumulative_ticks: 107,
        };
        let result = check_root_death(&server, &snapshot);
        // No connection → pid() is None → RootDied
        assert_eq!(result, Some(SettleResult::RootDied));
    }
}
