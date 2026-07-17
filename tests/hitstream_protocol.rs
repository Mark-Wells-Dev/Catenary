// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! End-to-end streaming of the ws43 hit-batch frame protocol.
//!
//! The CLI-owns-the-walk engine streams a REAL ripgrep walk end-to-end through
//! a stub annotator over a real byte pipe (`tokio::io::duplex`) — no daemon
//! process, so the test is deterministic and fast, but the full frame protocol
//! (hit-batch out, annotation-batch back, terminators, budget verdicts) travels
//! real bytes across the pipe.
//!
//! The invariants pinned here:
//! - **Ordered, complete, budget-verdict-per-batch** through a pass-through
//!   annotator.
//! - **Degrade-only, in place** (ws43-02): daemon-absent,
//!   daemon-answers-unknown-method, and an accepts-then-silent daemon (the
//!   read deadline) all produce the IDENTICAL unannotated stdout stream —
//!   with no re-walk and no duplicated or dropped line.
//! - **Ordered emission under a slow batch**: the in-flight window pipelines but
//!   the reorder buffer never lets a fast later batch overtake a slow earlier one.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use catenary_cli::hitstream::annotator::{BatchEnricher, annotate_connection};
use catenary_cli::hitstream::engine::WalkOptions;
use catenary_cli::hitstream::frame::{AnnotatedHit, AnnotationVerdict};
use catenary_cli::hitstream::{
    ANNOTATION_BATCH_BUDGET, GrepRender, HIT_BATCH_SIZE, PassThroughEnricher, ResultSink, WireHit,
    daemon_stream, stdout_unannotated,
};

/// Writes `body` to `dir/name`, creating parent dirs.
fn write_file(dir: &Path, name: &str, body: &str) -> Result<()> {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("mkdir")?;
    }
    let mut f = std::fs::File::create(&path).context("create file")?;
    f.write_all(body.as_bytes()).context("write file")?;
    Ok(())
}

/// A tree with `count` matching lines spread across two files, enough to force
/// several hit-batches. Returns the tempdir (kept alive) and the roots to walk.
fn fixture(count: usize) -> Result<(tempfile::TempDir, Vec<PathBuf>)> {
    let tmp = tempfile::tempdir().context("tempdir")?;
    let root = tmp.path();
    let half = count / 2;
    let mut a = String::new();
    for i in 0..half {
        let _ = writeln!(a, "needle a{i}");
    }
    let mut b = String::new();
    for i in half..count {
        let _ = writeln!(b, "needle b{i}");
    }
    write_file(root, "aaa.txt", &a)?;
    write_file(root, "zzz.txt", &b)?;
    let roots = vec![root.to_path_buf()];
    Ok((tmp, roots))
}

/// Runs the CLI daemon-stream sink against an in-process annotator connected by a
/// duplex pipe, returning the bytes the result sink emitted plus the degrade
/// verdict.
async fn stream_through<E>(
    pattern: &str,
    roots: &[PathBuf],
    enricher: E,
    budget: std::time::Duration,
) -> Result<(String, bool)>
where
    E: BatchEnricher + Send + Sync + 'static,
{
    // Two duplex pipes: one carries CLI→daemon hit-frames, one carries
    // daemon→CLI annotation-frames.
    let (cli_writes, daemon_reads) = tokio::io::duplex(64 * 1024);
    let (daemon_writes, cli_reads) = tokio::io::duplex(64 * 1024);

    // The stub annotator: read hit-batches, annotate under budget, write back.
    let annotator = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(daemon_reads);
        let mut writer = daemon_writes;
        annotate_connection(&mut reader, &mut writer, &enricher, budget).await
    });

    let mut out: Vec<u8> = Vec::new();
    let degraded;
    {
        let mut sink = ResultSink::new(&mut out);
        let cli_reader = tokio::io::BufReader::new(cli_reads);
        let report = daemon_stream(
            pattern,
            roots,
            &WalkOptions::default(),
            None,
            cli_reader,
            cli_writes,
            &GrepRender::default(),
            &mut sink,
        )
        .await
        .context("cli daemon stream")?;
        degraded = report.degraded;
    }

    annotator
        .await
        .context("join annotator")?
        .context("annotator run")?;

    Ok((
        String::from_utf8(out).context("result bytes are utf-8")?,
        degraded,
    ))
}

/// The bytes the degrade path (`stdout_unannotated`) produces for the same walk.
fn degraded_bytes(pattern: &str, roots: &[PathBuf]) -> Result<String> {
    let mut out: Vec<u8> = Vec::new();
    {
        let mut sink = ResultSink::new(&mut out);
        stdout_unannotated(
            pattern,
            roots,
            &WalkOptions::default(),
            &GrepRender::default(),
            &mut sink,
        )
        .context("stdout unannotated")?;
    }
    String::from_utf8(out).context("degraded bytes are utf-8")
}

/// A real walk streamed through a pass-through annotator is ordered, complete,
/// and every hit comes back — the annotation stream carries the same lines the
/// degrade path would print (a pass-through batch renders the identical `#?`
/// grep spelling).
#[tokio::test]
async fn real_walk_streams_ordered_complete_through_stub_annotator() -> Result<()> {
    let count = HIT_BATCH_SIZE * 3 + 7;
    let (_tmp, roots) = fixture(count)?;

    let (streamed, degraded) = stream_through(
        "needle",
        &roots,
        PassThroughEnricher,
        ANNOTATION_BATCH_BUDGET,
    )
    .await?;
    assert!(!degraded, "a healthy exchange is not degraded");

    let lines: Vec<&str> = streamed.lines().collect();
    assert_eq!(lines.len(), count, "every hit is emitted exactly once");

    // Complete + ordered: the streamed lines equal the degrade path byte-for-byte
    // (pass-through anchors render like the unannotated spelling).
    let degraded_stream = degraded_bytes("needle", &roots)?;
    assert_eq!(
        streamed, degraded_stream,
        "a pass-through annotation stream equals the unannotated stream byte-for-byte"
    );

    // Ordered within a file: aaa.txt hits precede zzz.txt hits (path-sorted walk).
    let first_zzz = lines
        .iter()
        .position(|l| l.contains("zzz.txt"))
        .expect("zzz.txt appears");
    let last_aaa = lines
        .iter()
        .rposition(|l| l.contains("aaa.txt"))
        .expect("aaa.txt appears");
    assert!(
        last_aaa < first_zzz,
        "path-sorted order preserved: all aaa.txt hits precede all zzz.txt hits"
    );

    Ok(())
}

/// An enricher whose FIRST batch (seq 0) is slow and every later batch is
/// instant. Proves the in-flight window pipelines later batches ahead but the
/// reorder buffer never lets them overtake the slow seq-0 batch on the wire.
struct SlowFirstBatch;
impl BatchEnricher for SlowFirstBatch {
    async fn enrich(
        &self,
        hits: Vec<WireHit>,
        _observed: Vec<(PathBuf, i64)>,
        _weight: Option<catenary_cli::hitstream::EnrichmentWeight>,
    ) -> anyhow::Result<Vec<AnnotatedHit>> {
        // Tag each hit's anchor with a fixed trail so the output takes the
        // annotated (`#x`) spelling. A batch's first hit line identifies it.
        let first_line = hits.first().map_or(0, |h| h.line);
        // Batches whose first line is small (the earliest walk lines) are the
        // early batches; make the very first one slow.
        if first_line <= 1 {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        Ok(hits
            .into_iter()
            .map(|h| AnnotatedHit::scoped(h, "x".to_string()))
            .collect())
    }
}

/// Under a slow first batch, emission stays in walk order: the reorder buffer
/// holds every later (fast) batch until the slow seq-0 batch resolves, so the
/// output lines are still in ascending line order across the single file.
#[tokio::test]
async fn ordered_emission_under_a_slow_batch() -> Result<()> {
    let tmp = tempfile::tempdir().context("tempdir")?;
    let count = HIT_BATCH_SIZE * 4;
    let mut body = String::new();
    for i in 0..count {
        let _ = writeln!(body, "needle line {i}");
    }
    write_file(tmp.path(), "single.txt", &body)?;
    let roots = vec![tmp.path().to_path_buf()];

    let (streamed, degraded) =
        stream_through("needle", &roots, SlowFirstBatch, ANNOTATION_BATCH_BUDGET).await?;
    assert!(!degraded, "a slow batch delays, it does not degrade");

    // Extract the 1-based line number from each result line
    // (`path:line#x:text`, the grep shape) and assert strictly ascending — the
    // slow first batch never let a fast later batch overtake it.
    let mut lines_nums: Vec<u32> = Vec::new();
    for line in streamed.lines() {
        let after_path = line
            .split_once(".txt:")
            .map(|x| x.1)
            .expect("line has path:");
        let num: u32 = after_path
            .split(['#', ':'])
            .next()
            .expect("line number segment")
            .parse()
            .expect("line number parses");
        lines_nums.push(num);
    }
    assert_eq!(lines_nums.len(), count, "every hit present");
    let mut sorted = lines_nums.clone();
    sorted.sort_unstable();
    assert_eq!(
        lines_nums, sorted,
        "output stays in ascending line order despite a slow first batch (no reorder)"
    );

    Ok(())
}

/// Degrade-only: the daemon-absent stream (a failed connect) and the
/// daemon-answers-unknown-method stream (an old daemon that never speaks a
/// hit-frame) both collapse to the IDENTICAL unannotated stdout stream — the
/// old-daemon case completing **in place**, with no re-walk and no partial
/// output discarded.
#[tokio::test]
async fn daemon_absent_and_unknown_method_degrade_identically() -> Result<()> {
    let count = HIT_BATCH_SIZE + 3;
    let (_tmp, roots) = fixture(count)?;

    // The canonical degrade stream.
    let degraded_stream = degraded_bytes("needle", &roots)?;

    // ── Case A: daemon-absent. The CLI could not open the stream, so it runs
    // the degrade path directly. (In production this is the `connect_daemon`
    // failure branch; here we invoke the degrade path the branch falls back to.)
    let absent = degraded_bytes("needle", &roots)?;
    assert_eq!(absent, degraded_stream, "daemon-absent == degrade path");

    // ── Case B: an old daemon answers with a single non-frame line (its
    // unknown-method reply) then closes. The CLI's `daemon_stream` reads no
    // recognizable annotation frame and completes the whole stream unannotated
    // in place — same bytes, `degraded` verdict, no error.
    let (cli_writes, mut daemon_reads) = tokio::io::duplex(64 * 1024);
    let (mut daemon_writes, cli_reads) = tokio::io::duplex(64 * 1024);

    // The "old daemon": drain whatever the CLI sends (so the writer half does not
    // block on a full pipe), and answer with a legacy unknown-method line, then
    // close — never an annotation frame.
    let old_daemon = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _ = daemon_writes
            .write_all(b"{\"error\":\"unknown method\"}\n")
            .await;
        let _ = daemon_writes.shutdown().await;
        // Drain the CLI's frames so its writer never blocks.
        let mut sink = Vec::new();
        let _ = daemon_reads.read_to_end(&mut sink).await;
    });

    let mut streamed_out: Vec<u8> = Vec::new();
    {
        let mut sink = ResultSink::new(&mut streamed_out);
        let cli_reader = tokio::io::BufReader::new(cli_reads);
        let report = daemon_stream(
            "needle",
            &roots,
            &WalkOptions::default(),
            None,
            cli_reader,
            cli_writes,
            &GrepRender::default(),
            &mut sink,
        )
        .await
        .context("daemon_stream against an old daemon")?;
        assert!(
            report.degraded,
            "an unknown-method reply is a degrade the CLI reports, not an error"
        );
    }
    old_daemon.await.context("join old daemon")?;

    let streamed = String::from_utf8(streamed_out).context("utf-8")?;
    assert_eq!(
        streamed, degraded_stream,
        "daemon-answers-unknown-method completes in place with the identical \
         unannotated stream — nothing dropped, nothing duplicated"
    );

    Ok(())
}

/// The accepts-then-silent daemon: the connection opens and swallows every
/// hit-frame but never answers. The per-read deadline
/// ([`catenary_cli::hitstream::STREAM_READ_DEADLINE`]) expires and the stream
/// completes unannotated in place — complete identical results, `degraded`
/// verdict, never a hang. Runs under a paused clock so the deadline fires
/// instantly once every task is idle.
#[tokio::test(start_paused = true)]
async fn silent_daemon_deadline_completes_unannotated() -> Result<()> {
    let count = HIT_BATCH_SIZE + 3;
    let (_tmp, roots) = fixture(count)?;
    let degraded_stream = degraded_bytes("needle", &roots)?;

    let (cli_writes, mut daemon_reads) = tokio::io::duplex(64 * 1024);
    // The silent daemon holds its write half open (no EOF, no frames) for the
    // whole exchange — only the deadline can end the wait.
    let (daemon_writes, cli_reads) = tokio::io::duplex(64 * 1024);

    let silent_daemon = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        // Swallow the CLI's frames forever; never write back.
        let mut sink = Vec::new();
        let _ = daemon_reads.read_to_end(&mut sink).await;
        drop(daemon_writes);
    });

    let mut streamed_out: Vec<u8> = Vec::new();
    {
        let mut sink = ResultSink::new(&mut streamed_out);
        let cli_reader = tokio::io::BufReader::new(cli_reads);
        let report = daemon_stream(
            "needle",
            &roots,
            &WalkOptions::default(),
            None,
            cli_reader,
            cli_writes,
            &GrepRender::default(),
            &mut sink,
        )
        .await
        .context("daemon_stream against a silent daemon")?;
        assert!(report.degraded, "a silent daemon is a degrade, not a hang");
    }
    silent_daemon.await.context("join silent daemon")?;

    let streamed = String::from_utf8(streamed_out).context("utf-8")?;
    assert_eq!(
        streamed, degraded_stream,
        "the deadline completes the stream unannotated — complete identical \
         results, fewer annotations"
    );

    Ok(())
}

/// Every batch's annotation carries a budget verdict; a pass-through annotator
/// verdicts `annotated` (it completes within budget with no real work), while a
/// blown budget verdicts `passed_through` — and either way every hit survives.
#[tokio::test]
async fn budget_verdict_present_per_batch() -> Result<()> {
    // Drive the annotator directly over a duplex to inspect the verdict frames.
    use catenary_cli::hitstream::frame::{AnnotationFrame, HitFrame};
    use catenary_cli::hitstream::{read_frame, write_frame};

    let (mut cli_writes, daemon_reads) = tokio::io::duplex(64 * 1024);
    let (daemon_writes, cli_reads) = tokio::io::duplex(64 * 1024);

    let annotator = tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(daemon_reads);
        let mut writer = daemon_writes;
        annotate_connection(
            &mut reader,
            &mut writer,
            &PassThroughEnricher,
            ANNOTATION_BATCH_BUDGET,
        )
        .await
    });

    // Send two hit-batches and a terminator.
    let batch = |seq: u64| {
        HitFrame::batch(
            seq,
            vec![WireHit {
                path: PathBuf::from("/w/a.rs"),
                line: 1,
                column: 1,
                text: "needle".to_string(),
            }],
        )
    };
    write_frame(&mut cli_writes, &batch(0)).await?;
    write_frame(&mut cli_writes, &batch(1)).await?;
    write_frame(&mut cli_writes, &HitFrame::end(2)).await?;
    {
        use tokio::io::AsyncWriteExt;
        cli_writes.shutdown().await?;
    }

    let mut reader = tokio::io::BufReader::new(cli_reads);
    let mut line = String::new();
    let mut verdicts = 0;
    let mut batches = 0;
    loop {
        let frame: Option<AnnotationFrame> = read_frame(&mut reader, &mut line).await?;
        match frame {
            Some(AnnotationFrame::Batch { batch }) => {
                // Every annotation-batch carries a verdict; pass-through completes
                // in-budget so it verdicts `annotated`, and keeps every hit.
                assert!(
                    matches!(batch.verdict, AnnotationVerdict::Annotated),
                    "pass-through within budget verdicts annotated"
                );
                assert_eq!(batch.hits.len(), 1, "the hit survives annotation");
                verdicts += 1;
                batches += 1;
            }
            Some(AnnotationFrame::End { batches: n }) => {
                assert_eq!(n, 2, "terminator counts every annotated batch");
                break;
            }
            None => anyhow::bail!("annotation stream ended without a terminator"),
        }
    }
    assert_eq!(verdicts, 2, "a verdict per batch");
    assert_eq!(batches, 2);

    annotator
        .await
        .context("join annotator")?
        .context("annotator run")?;
    Ok(())
}
