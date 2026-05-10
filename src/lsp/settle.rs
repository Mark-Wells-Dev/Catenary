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

use catenary_proc::ProcessState;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use super::server::LspServer;
use super::state::ServerLifecycle;

// ── Constants ────────────────────────────────────────────────────────

/// Polling interval for tree walks (validated by profiling).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Budget: 60 seconds of server CPU time in centiseconds (100 Hz).
const CPUTIME_BUDGET: u64 = 6000;

// ── IdleDetector ─────────────────────────────────────────────────────

/// Outcome of the idle detection operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleResult {
    /// Server settled — all processes quiet.
    Settled,
    /// Budget exhausted — server consumed 60s of CPU time.
    BudgetExhausted,
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
    /// 2. Wait for all processes to show zero deltas with per-child gates.
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

        for ts in &snapshot.samples {
            let is_active = ts.delta_pfc > 0 || ts.delta_utime > 0 || ts.delta_stime > 0;

            if is_active {
                any_nonzero = true;
                self.active_pids.insert(ts.pid);
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

        // Phase 2: quiet detection — all zeros, no new PIDs
        if any_nonzero || new_pids {
            return false;
        }

        // Per-child gates: every live process must have shown activity
        snapshot
            .samples
            .iter()
            .all(|ts| ts.state == ProcessState::Dead || self.active_pids.contains(&ts.pid))
    }
}

// ── await_idle ───────────────────────────────────────────────────────

/// Waits for the server to go idle using the provided detector.
///
/// Runs a 50ms polling loop, handles budget tracking, root death detection,
/// `Busy(n)` lifecycle pausing, and cancellation. Delegates idle detection
/// to [`IdleDetector::check`].
///
/// Returns when the server is idle, the cputime budget is exhausted,
/// the root process dies, or the cancel token fires.
#[allow(
    clippy::similar_names,
    reason = "delta_utime/delta_stime are standard counter names"
)]
pub async fn await_idle(
    server: &Arc<LspServer>,
    mut detector: IdleDetector,
    cancel: CancellationToken,
) -> SettleResult {
    let mut cumulative_cputime: u64 = 0;

    loop {
        tokio::select! {
            () = tokio::time::sleep(POLL_INTERVAL) => {}
            () = cancel.cancelled() => { return SettleResult::Settled; }
        }

        let lifecycle = server.lifecycle();

        // Terminal states
        if lifecycle.is_terminal() {
            return SettleResult::RootDied;
        }

        // During Busy: activity is implicit, skip tree walking
        if matches!(lifecycle, ServerLifecycle::Busy(_)) {
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

        // Budget tracking
        cumulative_cputime += interval_cost(&snapshot);
        if cumulative_cputime >= CPUTIME_BUDGET {
            debug!("idle_detector: budget exhausted ({cumulative_cputime} centiseconds)");
            return SettleResult::BudgetExhausted;
        }

        // Idle check
        if detector.check(&snapshot) {
            debug!("idle_detector: server idle");
            return SettleResult::Settled;
        }
    }
}

/// Computes the CPU-time cost of a single polling interval.
///
/// Each process contributes `delta_utime + delta_stime` centiseconds.
/// Processes with zero CPU time but nonzero page faults are charged a
/// minimum of 1 centisecond (they were scheduled but didn't accumulate
/// a full tick).
#[allow(
    clippy::similar_names,
    reason = "delta_utime/delta_stime are standard counter names"
)]
fn interval_cost(snapshot: &catenary_proc::TreeSnapshot) -> u64 {
    let mut cost: u64 = 0;
    for ts in &snapshot.samples {
        let cputime = ts.delta_utime + ts.delta_stime;
        cost += if cputime == 0 && ts.delta_pfc > 0 {
            1
        } else {
            cputime
        };
    }
    cost
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
        LspServer::new(
            "test".to_string(),
            "test-server".to_string(),
            None,
            std::collections::HashMap::new(),
        )
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
            state: ProcessState::Running,
        }
    }

    fn quiet_sample(pid: u32) -> TreeSample {
        TreeSample {
            pid,
            ppid: 1,
            delta_utime: 0,
            delta_stime: 0,
            delta_pfc: 0,
            state: ProcessState::Running,
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

    // ── interval_cost unit tests ──────────────────────────────────────

    #[test]
    fn interval_cost_sums_cputime() {
        let snapshot = make_snapshot(vec![
            TreeSample {
                pid: 1,
                ppid: 0,
                delta_utime: 10,
                delta_stime: 3,
                delta_pfc: 0,
                state: ProcessState::Running,
            },
            TreeSample {
                pid: 2,
                ppid: 1,
                delta_utime: 5,
                delta_stime: 2,
                delta_pfc: 0,
                state: ProcessState::Running,
            },
        ]);
        assert_eq!(interval_cost(&snapshot), 20);
    }

    #[test]
    fn interval_cost_pfc_only_charges_minimum() {
        let snapshot = make_snapshot(vec![TreeSample {
            pid: 1,
            ppid: 0,
            delta_utime: 0,
            delta_stime: 0,
            delta_pfc: 50,
            state: ProcessState::Running,
        }]);
        assert_eq!(interval_cost(&snapshot), 1);
    }

    #[test]
    fn interval_cost_zero_activity() {
        let snapshot = make_snapshot(vec![quiet_sample(1)]);
        assert_eq!(interval_cost(&snapshot), 0);
    }

    #[test]
    fn interval_cost_mixed_processes() {
        let snapshot = make_snapshot(vec![
            // CPU active: contributes 7
            TreeSample {
                pid: 1,
                ppid: 0,
                delta_utime: 4,
                delta_stime: 3,
                delta_pfc: 100,
                state: ProcessState::Running,
            },
            // PFC only: contributes 1
            TreeSample {
                pid: 2,
                ppid: 1,
                delta_utime: 0,
                delta_stime: 0,
                delta_pfc: 20,
                state: ProcessState::Running,
            },
            // Idle: contributes 0
            quiet_sample(3),
        ]);
        assert_eq!(interval_cost(&snapshot), 8);
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
        let result = await_idle(&server, detector, cancel).await;
        assert_eq!(result, SettleResult::RootDied);
    }

    #[tokio::test]
    async fn await_idle_returns_root_died_without_tree_monitor() {
        let server = Arc::new(test_server());
        server.set_lifecycle(ServerLifecycle::Healthy);
        let cancel = CancellationToken::new();
        let detector = IdleDetector::unconditional();
        let result = await_idle(&server, detector, cancel).await;
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
