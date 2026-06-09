// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
#![allow(
    clippy::print_stdout,
    reason = "benchmark test prints throughput results"
)]
#![allow(
    clippy::cast_precision_loss,
    reason = "precision loss is acceptable for display-only benchmark stats"
)]
//! Integration benchmark for LSP message throughput via `LoggingServer`.
//!
//! Spawns mockls, fires rounds of hover + definition requests, and measures
//! wall-clock elapsed and event-capture throughput (messages/sec). Run with:
//!
//! ```text
//! make test T=logging_throughput
//! ```

use std::time::Instant;

use anyhow::Result;

use catenary_mcp::logging::test_support::{message_count, setup_logging, spawn_initialized_client};

const MOCK_LANG_A: &str = "yX4Za";

/// End-to-end benchmark: hover + definition requests through mockls with
/// `LoggingServer` + the in-memory `MessageRecorder` test sink.
///
/// Prints throughput numbers. Does not assert a hard regression bound
/// because absolute timing varies by machine — the synthetic bench
/// (`benches/logging_overhead.rs`) covers the per-event overhead budget.
#[tokio::test]
async fn bench_lsp_message_throughput() -> Result<()> {
    let (logging, conn, _guard) = setup_logging();
    let (client, dir) =
        spawn_initialized_client(env!("CARGO_BIN_EXE_mockls"), logging, MOCK_LANG_A).await?;

    let file = dir.path().join(format!("test.{MOCK_LANG_A}"));
    std::fs::write(&file, "let MY_VAR\n")?;
    let uri = format!("file://{}", file.display());
    client
        .did_open(&uri, MOCK_LANG_A, 1, "let MY_VAR\n")
        .await?;

    // Baseline message count (initialize + didOpen).
    let pre_count = message_count(&conn);

    let rounds = 100;
    let start = Instant::now();
    for _ in 0..rounds {
        let _def = client.definition(&uri, 0, 4).await?;
        let _refs = client.references(&uri, 0, 4, true).await?;
    }
    let elapsed = start.elapsed();

    let post_count = message_count(&conn);
    let tool_msgs = post_count - pre_count;

    println!();
    println!("LSP message throughput (mockls, in-memory recorder)");
    println!("==============================================");
    println!("Rounds:           {rounds} (hover + definition each)");
    println!("Elapsed:          {:.2} ms", elapsed.as_secs_f64() * 1000.0);
    println!("Messages logged:  {tool_msgs}");
    println!(
        "Throughput:       {:.0} msgs/sec",
        tool_msgs as f64 / elapsed.as_secs_f64()
    );
    println!(
        "Per-round:        {:.2} ms",
        elapsed.as_secs_f64() * 1000.0 / f64::from(rounds)
    );

    // Sanity: each request/response pair is 2 messages, 2 methods per round.
    // Minimum expected: rounds × 2 methods × 2 messages = 400.
    assert!(
        tool_msgs >= i64::from(rounds * 2 * 2),
        "expected at least {} messages, got {tool_msgs}",
        rounds * 2 * 2,
    );

    Ok(())
}
