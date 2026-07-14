// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![allow(clippy::print_stdout, reason = "benchmark outputs results to stdout")]
#![allow(clippy::expect_used, reason = "benchmark setup uses expect")]
#![allow(
    clippy::cast_precision_loss,
    reason = "precision loss is acceptable for display-only benchmark stats"
)]

//! Synthetic benchmark for `LoggingServer` Layer dispatch overhead.
//!
//! Measures the per-event cost of emitting `tracing::info!` events through
//! `LoggingServer` with 0, 1, 2, and 4 no-op sinks. Uses `std::time::Instant`
//! (no `criterion` dependency). Run via `cargo bench --bench logging_overhead`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use catenary_cli::logging::{LogEvent, LoggingServer, Sink};
use tracing_subscriber::layer::SubscriberExt;

/// No-op sink for measuring pure dispatch overhead.
struct NoopSink;

impl Sink for NoopSink {
    fn handle(&self, _event: &LogEvent<'_>) {}
}

const EVENTS_PER_ITER: usize = 10_000;
const WARMUP_ITERS: usize = 5;
const BENCH_ITERS: usize = 21;

/// Benchmark Layer dispatch with `n` no-op sinks. Returns the median duration
/// across `BENCH_ITERS` iterations of `EVENTS_PER_ITER` events each.
fn bench_n_sinks(n: usize) -> Duration {
    let server = LoggingServer::new();
    let sinks: Vec<Arc<dyn Sink>> = (0..n)
        .map(|_| Arc::new(NoopSink) as Arc<dyn Sink>)
        .collect();
    server.activate(sinks);

    let subscriber = tracing_subscriber::registry().with(server);
    let dispatch = tracing::Dispatch::new(subscriber);

    // Warmup — prime caches and branch prediction.
    for _ in 0..WARMUP_ITERS {
        tracing::dispatcher::with_default(&dispatch, || {
            for _ in 0..EVENTS_PER_ITER {
                tracing::info!(
                    kind = "lsp",
                    method = "textDocument/hover",
                    server = "rust-analyzer",
                    request_id = 1_i64,
                    payload = "{}",
                    "outgoing"
                );
            }
        });
    }

    // Measure.
    let mut times = Vec::with_capacity(BENCH_ITERS);
    for _ in 0..BENCH_ITERS {
        let start = Instant::now();
        tracing::dispatcher::with_default(&dispatch, || {
            for _ in 0..EVENTS_PER_ITER {
                tracing::info!(
                    kind = "lsp",
                    method = "textDocument/hover",
                    server = "rust-analyzer",
                    request_id = 1_i64,
                    payload = "{}",
                    "outgoing"
                );
            }
        });
        times.push(start.elapsed());
    }

    times.sort();
    times[BENCH_ITERS / 2]
}

fn main() {
    println!("LoggingServer Layer dispatch overhead");
    println!("=====================================");
    println!(
        "{EVENTS_PER_ITER} events/iter, {BENCH_ITERS} iterations (median), {WARMUP_ITERS} warmup"
    );
    println!();

    let baseline = bench_n_sinks(0);
    let baseline_ns = baseline.as_nanos() as f64 / EVENTS_PER_ITER as f64;

    println!(
        "0 sink(s): {:>8.2} ms total, {:>6.0} ns/event (baseline)",
        baseline.as_secs_f64() * 1000.0,
        baseline_ns,
    );

    for n in [1, 2, 4] {
        let median = bench_n_sinks(n);
        let ns_per_event = median.as_nanos() as f64 / EVENTS_PER_ITER as f64;
        let overhead_pct = ((ns_per_event - baseline_ns) / baseline_ns) * 100.0;
        println!(
            "{n} sink(s): {:>8.2} ms total, {:>6.0} ns/event ({overhead_pct:>+.1}%)",
            median.as_secs_f64() * 1000.0,
            ns_per_event,
        );
    }

    println!();
    println!("Budget: ≤10% regression per sink added.");
}
