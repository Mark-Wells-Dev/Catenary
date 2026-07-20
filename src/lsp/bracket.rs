// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Transaction brackets: serialized, run-to-completion access to an LSP
//! server instance (brackets 02).
//!
//! A **bracket** is one consumer's open → request(s) → answer → close
//! against a single server instance, run to completion. While a bracket
//! runs, that consumer owns the instance's document state end to end —
//! concurrent consumers of the same instance queue behind it instead of
//! interleaving `didOpen`/request/`didClose` traffic on the shared session.
//!
//! Shape of the machinery:
//!
//! - **One queue per [`InstanceKey`]**, never global. The registry lock in
//!   [`BracketQueues`] is lookup-scoped only — held to find or create the
//!   per-instance queue, released before any enqueue or bracket work.
//!   Nothing above an instance's own queue is ever held across a bracket,
//!   so a bracket on one instance can never delay work on another (the
//!   bug-104 invariant).
//! - **Two lanes per queue** ([`Lane`]): debt-payment serves ahead of
//!   enrichment. Lane priority applies only at transaction boundaries —
//!   when a bracket completes, the next is chosen debt-first. A running
//!   bracket is never interrupted or preempted.
//! - **A generous service budget** ([`BRACKET_SERVICE_BUDGET`]) as the
//!   pathology backstop, never a tight wall clock. Budget expiry is a
//!   *completed-degraded* transaction: the teardown leg still runs, the
//!   close still happens, and the answer degrades to raw
//!   ([`BracketOutcome::Degraded`]). Whether a query enriches at all is
//!   decided capability-shaped ("does a capable server exist"), upstream
//!   of this module — the budget only bounds a wedged bracket.
//! - **Run to completion, always.** [`BracketQueues::run`] executes the
//!   bracket on a spawned task, so a caller abandoning its future (drop,
//!   cancellation) cannot abandon the bracket mid-flight — the body is
//!   bounded by the budget, the close leg runs, and the instance's queue
//!   is released.
//!
//! The type is instance-generic — queues key on the full [`InstanceKey`],
//! so extending brackets from the rootless single-file tier to root
//! instances is a matter of new call sites, not redesign.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

use crate::lsp::instance_key::InstanceKey;
use crate::source::Source;

/// Per-bracket service budget: the pathology backstop.
///
/// Generous by ruling (workstream "brackets"): concurrent `make check`-class
/// CPU contention is routine, and a tight budget would degrade exactly when
/// the machine is busy. A healthy bracket finishes orders of magnitude
/// sooner; only a genuinely wedged one is cut, and even then the transaction
/// completes degraded — teardown runs, the close happens, the answer
/// degrades to raw. Injectable via [`BracketQueues::with_budget`] so tests
/// drive expiry in milliseconds.
pub const BRACKET_SERVICE_BUDGET: Duration = Duration::from_mins(10);

/// Scheduling lane for a queued bracket.
///
/// Applied only at transaction boundaries: when a bracket completes, the
/// next one is drawn from the debt lane first. A running bracket is never
/// interrupted — lanes reorder waiters, they never preempt service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// Diagnostics-debt payment: serves ahead of enrichment.
    DebtPayment,
    /// Query enrichment: the default lane.
    Enrichment,
}

impl Lane {
    /// Returns the lane as a machine-readable string (telemetry field).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DebtPayment => "debt_payment",
            Self::Enrichment => "enrichment",
        }
    }
}

/// How a bracket concluded.
///
/// Both variants are *completed* transactions — the close leg ran and the
/// instance's queue was released. There is no abandoned variant by design:
/// mid-bracket abandonment is impossible.
#[derive(Debug, PartialEq, Eq)]
pub enum BracketOutcome<T> {
    /// The bracket ran to completion inside its service budget.
    Completed(T),
    /// The service budget expired (or the bracket task was lost): the
    /// bracket completed degraded — teardown ran, but the answer must
    /// degrade to raw.
    Degraded,
}

impl<T> BracketOutcome<T> {
    /// Returns the served value, or `None` for a degraded bracket.
    pub fn completed(self) -> Option<T> {
        match self {
            Self::Completed(value) => Some(value),
            Self::Degraded => None,
        }
    }

    /// Whether the bracket degraded (budget expiry backstop).
    #[must_use]
    pub const fn is_degraded(&self) -> bool {
        matches!(self, Self::Degraded)
    }
}

/// A queued waiter: the hand-off channel its bracket task awaits.
type Waiter = tokio::sync::oneshot::Sender<Baton>;

/// The two-lane wait state of one instance's queue.
struct QueueState {
    /// Whether a bracket currently holds the instance.
    busy: bool,
    /// Debt-payment lane: served first at every transaction boundary.
    debt: VecDeque<Waiter>,
    /// Enrichment lane: served when the debt lane is empty.
    enrichment: VecDeque<Waiter>,
}

/// One instance's bracket queue. Shared by every bracket ever run against
/// that [`InstanceKey`]; enqueue and release touch only this queue's own
/// lock — never the registry above it (bug 104).
struct QueueShared {
    state: std::sync::Mutex<QueueState>,
}

impl QueueShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(QueueState {
                busy: false,
                debt: VecDeque::new(),
                enrichment: VecDeque::new(),
            }),
        })
    }

    /// Acquires the instance for one bracket, waiting in `lane` if busy.
    ///
    /// Cancel-safe: a waiter dropped before hand-off is skipped at release
    /// time; a baton sent to a waiter that has since been dropped is
    /// re-released by the baton's own `Drop`, so the queue can never wedge
    /// on an abandoned waiter.
    async fn acquire(self: &Arc<Self>, lane: Lane) -> Baton {
        loop {
            let rx = {
                let mut state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.busy {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    match lane {
                        Lane::DebtPayment => state.debt.push_back(tx),
                        Lane::Enrichment => state.enrichment.push_back(tx),
                    }
                    Some(rx)
                } else {
                    state.busy = true;
                    None
                }
            };
            let Some(rx) = rx else {
                return Baton {
                    shared: Arc::clone(self),
                    armed: true,
                };
            };
            if let Ok(baton) = rx.await {
                return baton;
            }
            // Sender dropped without a hand-off — unreachable in practice
            // (waiters only leave the queue through a send), but never
            // deadlock on it: re-contend from the top.
        }
    }

    /// Releases the instance at a transaction boundary and hands the baton
    /// to the next waiter — debt lane first, enrichment second. A waiter
    /// that vanished (dropped receiver) is skipped; with no waiters left the
    /// queue goes idle.
    fn release(shared: &Arc<Self>) {
        let mut baton = Baton {
            shared: Arc::clone(shared),
            armed: true,
        };
        loop {
            let next = {
                let mut state = shared
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                // Lane priority lives here, at the boundary: debt first.
                let next = state
                    .debt
                    .pop_front()
                    .or_else(|| state.enrichment.pop_front());
                if next.is_none() {
                    state.busy = false;
                }
                drop(state);
                next
            };
            let Some(waiter) = next else {
                baton.armed = false;
                return;
            };
            match waiter.send(baton) {
                Ok(()) => return,
                // Receiver gone: take the baton back and try the next.
                Err(returned) => baton = returned,
            }
        }
    }
}

/// The held-instance token for one running bracket.
///
/// Dropping an armed baton releases the instance's queue — including a
/// baton that was handed to a since-dropped waiter, whose channel drops it.
struct Baton {
    shared: Arc<QueueShared>,
    armed: bool,
}

impl Drop for Baton {
    fn drop(&mut self) {
        if self.armed {
            self.armed = false;
            QueueShared::release(&self.shared);
        }
    }
}

/// The per-instance bracket queue registry.
///
/// One queue per [`InstanceKey`], created on first use and kept for the
/// registry's lifetime (a handful of `(language, server, scope)` identities
/// — respawns of the same identity share the queue, which keeps
/// serialization airtight across an instance's reap/respawn seam). The
/// registry's own lock is lookup-scoped only: held to fetch or insert a
/// queue handle, released before any enqueue, wait, or bracket work — a
/// bracket on one instance never blocks lookups or brackets on any other
/// (the bug-104 invariant).
pub struct BracketQueues {
    queues: std::sync::Mutex<HashMap<InstanceKey, Arc<QueueShared>>>,
    /// Per-bracket service budget (the pathology backstop).
    budget: Duration,
}

impl Default for BracketQueues {
    fn default() -> Self {
        Self::new()
    }
}

impl BracketQueues {
    /// Creates a registry with the production service budget
    /// ([`BRACKET_SERVICE_BUDGET`]).
    #[must_use]
    pub fn new() -> Self {
        Self::with_budget(BRACKET_SERVICE_BUDGET)
    }

    /// Creates a registry with an injected service budget.
    ///
    /// The injection seam for tests (millisecond budgets drive the expiry
    /// backstop deterministically). Production uses [`Self::new`] — budgets
    /// are constant and generous by ruling, never tuned at runtime.
    #[must_use]
    pub fn with_budget(budget: Duration) -> Self {
        Self {
            queues: std::sync::Mutex::new(HashMap::new()),
            budget,
        }
    }

    /// Fetches (or creates) the queue for `key`. Lookup-scoped registry
    /// lock: acquired, resolved, released — never held across enqueue or
    /// bracket work (bug 104).
    fn queue_for(&self, key: &InstanceKey) -> Arc<QueueShared> {
        let mut queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(queues.entry(key.clone()).or_insert_with(QueueShared::new))
    }

    /// Runs one transaction bracket against `key`'s instance.
    ///
    /// Waits in `lane` on the instance's own queue, then serves `body`
    /// (open → request(s) → answer) bounded by the service budget, then
    /// runs `close` (the teardown leg) unconditionally — budget expiry
    /// degrades the answer ([`BracketOutcome::Degraded`]), it never skips
    /// the close. The bracket executes on a spawned task: a caller that
    /// drops this future abandons only its view of the answer, never the
    /// bracket — body, close, and queue release still run to completion.
    ///
    /// Emits per-bracket telemetry to the firehose (`info!`): queue wait
    /// and service time, with the instance key, lane, and degraded flag as
    /// structured fields.
    pub async fn run<T, B, BFut, C, CFut>(
        &self,
        key: &InstanceKey,
        lane: Lane,
        body: B,
        close: C,
    ) -> BracketOutcome<T>
    where
        T: Send + 'static,
        B: FnOnce() -> BFut + Send + 'static,
        BFut: Future<Output = T> + Send + 'static,
        C: FnOnce() -> CFut + Send + 'static,
        CFut: Future<Output = ()> + Send + 'static,
    {
        // Registry touch ends here — the task below holds only the
        // instance's own queue handle.
        let queue = self.queue_for(key);
        let budget = self.budget;
        let task_key = key.clone();
        let handle = tokio::spawn(async move {
            let key = task_key;
            let enqueued = Instant::now();
            let permit = queue.acquire(lane).await;
            let queue_wait = enqueued.elapsed();

            let started = Instant::now();
            let served = tokio::time::timeout(budget, body()).await;
            // The teardown leg always runs: budget expiry is a
            // completed-degraded transaction, never an abandoned one.
            close().await;
            let service = started.elapsed();
            drop(permit);

            let outcome = match served {
                Ok(value) => BracketOutcome::Completed(value),
                Err(_expired) => BracketOutcome::Degraded,
            };
            info!(
                source = Source::LspDispatch.as_str(),
                server = key.server.as_str(),
                language = key.language_id.as_str(),
                scope = key.scope.kind_str(),
                lane = lane.as_str(),
                queue_wait_ms = u64::try_from(queue_wait.as_millis()).unwrap_or(u64::MAX),
                service_ms = u64::try_from(service.as_millis()).unwrap_or(u64::MAX),
                degraded = outcome.is_degraded(),
                "Bracket completed on {key}",
            );
            outcome
        });
        match handle.await {
            Ok(outcome) => outcome,
            Err(join_error) => {
                // Unreachable in practice: panics are forbidden crate-wide
                // and nothing aborts the bracket task. Degrade honestly
                // rather than pretend an answer exists.
                debug!(
                    source = Source::LspDispatch.as_str(),
                    server = key.server.as_str(),
                    language = key.language_id.as_str(),
                    "Bracket task lost on {key}: {join_error}",
                );
                BracketOutcome::Degraded
            }
        }
    }

    /// Lane depths `(debt, enrichment)` for `key`'s queue — test
    /// introspection so lane-ordering tests enqueue deterministically.
    #[cfg(test)]
    fn depths(&self, key: &InstanceKey) -> (usize, usize) {
        let queues = self
            .queues
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queues.get(key).map_or((0, 0), |queue| {
            let state = queue
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (state.debt.len(), state.enrichment.len())
        })
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use crate::lsp::instance_key::Scope;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::Notify;
    use tokio::time::{sleep, timeout};

    /// Shared event log: brackets record their legs; tests assert ordering.
    type Log = Arc<std::sync::Mutex<Vec<String>>>;

    fn push(log: &Log, event: &str) {
        log.lock().expect("log lock").push(event.to_string());
    }

    fn snapshot(log: &Log) -> Vec<String> {
        log.lock().expect("log lock").clone()
    }

    fn key(server: &str) -> InstanceKey {
        InstanceKey::new("lang".to_string(), server.to_string(), Scope::SingleFile)
    }

    /// Polls until `cond` holds (bounded), then asserts it.
    async fn wait_until(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..1000 {
            if cond() {
                return;
            }
            sleep(Duration::from_millis(2)).await;
        }
        assert!(cond(), "timed out waiting for: {what}");
    }

    #[tokio::test]
    async fn concurrent_brackets_on_one_instance_never_interleave() {
        // (a) Two concurrent consumers of one instance serialize: each
        // bracket's open → answer → close is contiguous in the event log,
        // in either order — never interleaved.
        let queues = Arc::new(BracketQueues::new());
        let k = key("srv");
        let log: Log = Log::default();

        let bracket = |tag: &'static str| {
            let body_log = log.clone();
            let close_log = log.clone();
            (
                move || async move {
                    push(&body_log, &format!("{tag}:open"));
                    // Yield generously so an unserialized peer WOULD
                    // interleave here.
                    sleep(Duration::from_millis(20)).await;
                    push(&body_log, &format!("{tag}:answer"));
                },
                move || async move {
                    push(&close_log, &format!("{tag}:close"));
                },
            )
        };

        let (body_a, close_a) = bracket("a");
        let (body_b, close_b) = bracket("b");
        let (out_a, out_b) = tokio::join!(
            queues.run(&k, Lane::Enrichment, body_a, close_a),
            queues.run(&k, Lane::Enrichment, body_b, close_b),
        );
        assert_eq!(out_a, BracketOutcome::Completed(()));
        assert_eq!(out_b, BracketOutcome::Completed(()));

        let events = snapshot(&log);
        assert_eq!(events.len(), 6, "two full brackets: {events:?}");
        for chunk in events.chunks(3) {
            let tag = chunk[0].split(':').next().expect("tag");
            let expected: Vec<String> = ["open", "answer", "close"]
                .iter()
                .map(|leg| format!("{tag}:{leg}"))
                .collect();
            assert_eq!(chunk, expected, "bracket interleaved: {events:?}");
        }
    }

    #[tokio::test]
    async fn bracket_on_one_instance_never_delays_another() {
        // (b) The bug-104 invariant: a bracket holding instance A does not
        // block a bracket on instance B — the queues are per-instance and
        // nothing above them is held across a bracket.
        let queues = Arc::new(BracketQueues::new());
        let key_a = key("srv-a");
        let key_b = key("srv-b");

        let hold = Arc::new(Notify::new());
        let a_started = Arc::new(Notify::new());
        let a_hold = hold.clone();
        let a_started_tx = a_started.clone();
        let queues_a = Arc::clone(&queues);
        let key_a_task = key_a.clone();
        let a_task = tokio::spawn(async move {
            queues_a
                .run(
                    &key_a_task,
                    Lane::Enrichment,
                    move || async move {
                        a_started_tx.notify_one();
                        a_hold.notified().await;
                    },
                    || async {},
                )
                .await
        });
        a_started.notified().await;

        // While A holds its instance, B must serve promptly.
        let out_b = timeout(
            Duration::from_secs(5),
            queues.run(&key_b, Lane::Enrichment, || async { 7 }, || async {}),
        )
        .await
        .expect("a bracket on B must not wait behind A's bracket");
        assert_eq!(out_b, BracketOutcome::Completed(7));

        hold.notify_one();
        let out_a = a_task.await.expect("join A");
        assert_eq!(out_a, BracketOutcome::Completed(()));
    }

    #[tokio::test]
    async fn debt_lane_serves_before_queued_enrichment_without_preemption() {
        // (c) Lane mechanics: with a bracket running and both lanes queued,
        // the boundary picks debt before the earlier-enqueued enrichment —
        // and the running bracket finishes first (reordering, never
        // interruption).
        let queues = Arc::new(BracketQueues::new());
        let k = key("srv");
        let log: Log = Log::default();

        let hold = Arc::new(Notify::new());
        let started = Arc::new(Notify::new());
        let spawn_bracket = |tag: &'static str, lane: Lane, gate: Option<Arc<Notify>>| {
            let queues = Arc::clone(&queues);
            let k = k.clone();
            let body_log = log.clone();
            let close_log = log.clone();
            let started = started.clone();
            tokio::spawn(async move {
                queues
                    .run(
                        &k,
                        lane,
                        move || async move {
                            push(&body_log, &format!("{tag}:serve"));
                            started.notify_one();
                            if let Some(gate) = gate {
                                gate.notified().await;
                            }
                        },
                        move || async move {
                            push(&close_log, &format!("{tag}:close"));
                        },
                    )
                    .await
            })
        };

        // e1 runs and holds the instance.
        let e1 = spawn_bracket("e1", Lane::Enrichment, Some(hold.clone()));
        started.notified().await;

        // e2 queues on the enrichment lane FIRST, then d on the debt lane.
        let e2 = spawn_bracket("e2", Lane::Enrichment, None);
        {
            let queues = Arc::clone(&queues);
            let k = k.clone();
            wait_until("e2 enqueued", move || queues.depths(&k) == (0, 1)).await;
        }
        let d = spawn_bracket("d", Lane::DebtPayment, None);
        {
            let queues = Arc::clone(&queues);
            let k = k.clone();
            wait_until("d enqueued", move || queues.depths(&k) == (1, 1)).await;
        }

        // Release the running bracket; the boundary reorders debt-first.
        hold.notify_one();
        for handle in [e1, d, e2] {
            let outcome = handle.await.expect("join bracket");
            assert_eq!(outcome, BracketOutcome::Completed(()));
        }
        assert_eq!(
            snapshot(&log),
            vec![
                "e1:serve", "e1:close", // the running bracket completes first
                "d:serve", "d:close", // debt jumps the earlier enrichment
                "e2:serve", "e2:close",
            ],
            "debt must serve at the boundary, ahead of queued enrichment",
        );
    }

    #[tokio::test]
    async fn budget_expiry_completes_degraded_and_still_closes() {
        // (d) The pathology backstop: a wedged body is cut at the injected
        // budget, but the transaction completes degraded — the close leg
        // runs and the queue is released for the next bracket.
        let queues = Arc::new(BracketQueues::with_budget(Duration::from_millis(50)));
        let k = key("srv");
        let closed = Arc::new(AtomicBool::new(false));

        let closed_tx = closed.clone();
        let outcome = queues
            .run(
                &k,
                Lane::Enrichment,
                std::future::pending::<()>,
                move || async move {
                    closed_tx.store(true, Ordering::SeqCst);
                },
            )
            .await;
        assert_eq!(outcome, BracketOutcome::Degraded);
        assert!(
            closed.load(Ordering::SeqCst),
            "the close leg must run on budget expiry",
        );

        // The queue released: the next bracket serves normally.
        let next = timeout(
            Duration::from_secs(5),
            queues.run(&k, Lane::DebtPayment, || async { 42 }, || async {}),
        )
        .await
        .expect("the expired bracket must release the queue");
        assert_eq!(next.completed(), Some(42));
    }

    #[tokio::test]
    async fn abandoned_caller_cannot_abandon_the_bracket() {
        // Run-to-completion: dropping the caller's future mid-bracket
        // abandons only the answer — body and close still complete on the
        // spawned task, and the queue is released.
        let queues = Arc::new(BracketQueues::new());
        let k = key("srv");
        let body_done = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));

        let body_done_tx = body_done.clone();
        let closed_tx = closed.clone();
        let abandoned = timeout(
            Duration::from_millis(5),
            queues.run(
                &k,
                Lane::Enrichment,
                move || async move {
                    sleep(Duration::from_millis(50)).await;
                    body_done_tx.store(true, Ordering::SeqCst);
                },
                move || async move {
                    closed_tx.store(true, Ordering::SeqCst);
                },
            ),
        )
        .await;
        assert!(abandoned.is_err(), "the caller abandoned its future");

        let closed_poll = closed.clone();
        wait_until("the abandoned bracket to complete", move || {
            closed_poll.load(Ordering::SeqCst)
        })
        .await;
        assert!(body_done.load(Ordering::SeqCst), "body ran to completion");
        assert!(closed.load(Ordering::SeqCst), "close ran to completion");

        // And the queue is free for the next bracket.
        let next = timeout(
            Duration::from_secs(5),
            queues.run(&k, Lane::Enrichment, || async { true }, || async {}),
        )
        .await
        .expect("queue released after the abandoned bracket");
        assert_eq!(next, BracketOutcome::Completed(true));
    }

    #[tokio::test]
    async fn telemetry_carries_queue_wait_and_service_time() {
        // (e) Per-bracket telemetry: one firehose event per bracket with
        // queue-wait and service time, keyed by instance and lane.
        let (_logging, recorder, _guard) = crate::logging::test_support::setup_logging();
        let queues = Arc::new(BracketQueues::new());
        let k = key("srv-telemetry");

        let outcome = queues
            .run(
                &k,
                Lane::DebtPayment,
                || async {
                    sleep(Duration::from_millis(15)).await;
                },
                || async {},
            )
            .await;
        assert_eq!(outcome, BracketOutcome::Completed(()));

        let rows = crate::logging::test_support::query_all_messages(&recorder);
        let row = rows
            .iter()
            .find(|r| r.payload.contains("queue_wait_ms"))
            .expect("a bracket telemetry event reaches the firehose sink");
        assert_eq!(row.level, "info", "internal diagnostics severity");
        assert_eq!(row.server, "srv-telemetry");

        let payload: serde_json::Value =
            serde_json::from_str(&row.payload).expect("payload is JSON");
        assert_eq!(payload["language"], "lang");
        let fields = payload["fields"]
            .as_object()
            .expect("structured fields present");
        assert_eq!(fields["lane"], "debt_payment");
        assert_eq!(fields["scope"], "single_file");
        assert_eq!(fields["degraded"], false);
        assert!(
            fields["queue_wait_ms"].is_u64(),
            "queue wait recorded: {fields:?}",
        );
        let service_ms = fields["service_ms"].as_u64().expect("service recorded");
        assert!(service_ms >= 15, "service covers the body: {service_ms}");
    }
}
