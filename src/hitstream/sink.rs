// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The two hit-batch sinks (ws43), selectable at the walk seam.
//!
//! - [`stdout_unannotated`] — the degrade path, built first. The walk's ordered
//!   hit-batches print straight to stdout with no daemon round-trip: exactly what
//!   a daemon-absent (or wedged, or old) `catenary grep` produces. Every result
//!   line is one atomic write.
//! - [`daemon_stream`] — batches out to the daemon, annotation-batches back,
//!   reassembled into batch-sequence order before emission. A small in-flight
//!   window pipelines the exchange without ever reordering output.
//!
//! ## Stream discipline, by construction
//!
//! [`ResultSink`] is the ONLY writer of a result line, and it frames each line as
//! a single buffered `write_all`. Advisories go to stderr through
//! [`super::advise`], a physically distinct stream. There is no shared buffer and
//! no partial-line window, so the class of bug where a stderr hint fuses into a
//! stdout result line under piping cannot occur: a result line is one atomic
//! write on one stream, an advisory is one atomic write on the other.

use anyhow::{Context, Result};

use super::frame::{AnnotatedBatch, AnnotationFrame, HitFrame};
use super::{HitBatch, IN_FLIGHT_WINDOW};

/// Connects to the daemon's IPC socket and opens the hit-batch annotation stream,
/// returning the buffered read half and the write half ready for
/// [`daemon_stream`].
///
/// Sends the `tool/hitstream` method line as the connection's first line (the
/// handshake `handle_hook_dispatch` reads to route the connection), then hands
/// back the split halves. This is the one place the method-line handshake lives,
/// so [`daemon_stream`] can stay a pure frame exchange (and be tested against a
/// stub annotator with no handshake).
///
/// # Errors
///
/// Returns an error if no daemon is listening (the caller's cue to degrade to
/// [`stdout_unannotated`]) or the handshake write fails.
#[cfg(unix)]
pub async fn connect_daemon(
    socket_path: &std::path::Path,
) -> Result<(
    tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
    tokio::net::unix::OwnedWriteHalf,
)> {
    use tokio::io::AsyncWriteExt;

    let stream = tokio::net::UnixStream::connect(socket_path)
        .await
        .context("connect daemon hitstream socket")?;
    let (reader, mut writer) = stream.into_split();

    let mut handshake =
        serde_json::to_vec(&serde_json::json!({ "method": super::HITSTREAM_METHOD }))
            .context("serialize hitstream handshake")?;
    handshake.push(b'\n');
    writer
        .write_all(&handshake)
        .await
        .context("write hitstream handshake")?;

    Ok((tokio::io::BufReader::new(reader), writer))
}

/// The sole writer of result lines to stdout.
///
/// Wraps a `std::io::Write` and guarantees that each result line is emitted as
/// one atomic `write_all` of the whole line (its newline included). Nothing else
/// writes to this stream, so no interleaving with an advisory or a partial line
/// is possible.
pub struct ResultSink<W: std::io::Write> {
    writer: W,
}

impl<W: std::io::Write> ResultSink<W> {
    /// Wraps `writer` as the result-line sink.
    pub const fn new(writer: W) -> Self {
        Self { writer }
    }

    /// Writes one complete result line, atomically.
    ///
    /// The line and its trailing newline are assembled into one owned buffer and
    /// flushed with a single `write_all` — the atomic-line-write leg of the
    /// stream-discipline invariant.
    ///
    /// # Errors
    ///
    /// Returns an error if the write fails.
    pub fn write_line(&mut self, line: &str) -> Result<()> {
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(line);
        buf.push('\n');
        self.writer
            .write_all(buf.as_bytes())
            .context("write result line")
    }

    /// Flushes the underlying writer.
    ///
    /// # Errors
    ///
    /// Returns an error if the flush fails.
    pub fn flush(&mut self) -> Result<()> {
        self.writer.flush().context("flush result sink")
    }
}

/// The degrade path (built first): print the walk's ordered hit-batches straight
/// to `sink`, unannotated.
///
/// Runs the walk with a streaming callback that renders each hit and writes it
/// through the [`ResultSink`] — no daemon, no buffered result set. The bytes are
/// identical to a pass-through annotation-batch (a `None`-anchor hit renders the
/// same spelling), which is exactly what makes daemon-absent, wedged-daemon, and
/// old-daemon degrade to the same stream.
///
/// Returns the walk's [`WalkSummary`] so the caller can report skips
/// (misc 135) — the CLI cutover folds them into the stderr skip lines.
///
/// # Errors
///
/// Returns an error if the walk fails to start (bad pattern) or a write fails.
pub fn stdout_unannotated<W: std::io::Write>(
    pattern: &str,
    roots: &[std::path::PathBuf],
    options: &super::engine::WalkOptions,
    sink: &mut ResultSink<W>,
) -> Result<super::engine::WalkSummary> {
    let summary = super::engine::walk(pattern, roots, options, |batch: HitBatch| {
        for hit in &batch.hits {
            sink.write_line(&hit.render_unannotated())?;
        }
        Ok(())
    })?;
    sink.flush()?;
    Ok(summary)
}

/// The daemon-stream sink: send hit-batches to the daemon, read annotation-batches
/// back, and emit them **in batch-sequence order** through `sink`.
///
/// The exchange is genuinely pipelined: the writer half (a spawned task) streams
/// hit-batches to the daemon while the reader half — the current task — drains
/// annotation-batches and emits. The two run concurrently, so neither side blocks
/// waiting for the other to finish; a bounded pipe cannot deadlock the way a
/// write-all-then-read-all sequence would. The in-flight window
/// ([`IN_FLIGHT_WINDOW`]) is the channel capacity that feeds the writer, so the
/// walk cannot race arbitrarily far ahead and buffer a whole result set (the
/// streaming, no-buffered-result-set invariant).
///
/// Because the daemon may resolve batches out of order (a slow batch finishes
/// after a later fast one), the reader **reassembles** annotation-batches into
/// `seq` order before writing a line — a slow batch delays emission but never
/// reorders it. Ordering here is by construction, not by trust: even if the
/// daemon returned annotation-batches shuffled, the reorder buffer restores `seq`
/// order.
///
/// On any stream fault (the peer errors or dies mid-exchange, or answers with an
/// unrecognizable frame — the old-daemon / version-skew signal) this function
/// surfaces the fault rather than emitting a partial-then-truncated stream; the
/// caller degrades to [`stdout_unannotated`], which produces the identical
/// unannotated result stream.
///
/// Returns the walk's [`WalkSummary`](super::engine::WalkSummary) so the caller
/// can report skips (misc 135) — the CLI cutover folds them into the stderr
/// skip lines.
///
/// # Errors
///
/// Returns an error if the walk fails to start, a frame read/write fails, or the
/// annotation stream is malformed or truncated. A returned error is the caller's
/// cue to fall back to the unannotated stream.
pub async fn daemon_stream<R, Wr, Wo>(
    pattern: &str,
    roots: &[std::path::PathBuf],
    options: &super::engine::WalkOptions,
    reader: R,
    writer: Wr,
    sink: &mut ResultSink<Wo>,
) -> Result<super::engine::WalkSummary>
where
    R: tokio::io::AsyncBufRead + Unpin,
    Wr: tokio::io::AsyncWrite + Unpin + Send + 'static,
    Wo: std::io::Write,
{
    use tokio::sync::mpsc;

    // The walk is CPU/IO-bound and synchronous (ripgrep). Run it on a blocking
    // task; hand each ordered batch to the async writer over a bounded channel
    // whose capacity IS the in-flight window — so the walk cannot race ahead and
    // buffer a whole result set (streaming invariant).
    let (batch_tx, mut batch_rx) = mpsc::channel::<HitBatch>(IN_FLIGHT_WINDOW);
    let pattern = pattern.to_string();
    let roots = roots.to_vec();
    let options = options.clone();

    let walk_task = tokio::task::spawn_blocking(move || -> Result<super::engine::WalkSummary> {
        super::engine::walk(&pattern, &roots, &options, |batch| {
            // A closed receiver (the writer died) aborts the walk — no point
            // reading the tree for a stream nobody is draining.
            batch_tx
                .blocking_send(batch)
                .map_err(|_| anyhow::anyhow!("annotation stream closed"))
        })
    });

    // Writer half runs CONCURRENTLY with the reader below, so the daemon can
    // drain hit-frames and answer annotation-frames while the CLI is still
    // walking — the pipeline. Draining the walk channel, framing each batch, and
    // the terminator all live in this task; it half-closes the write side at the
    // end so the daemon sees EOF on its read loop.
    let writer_task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut writer = writer;
        let mut sent: u64 = 0;
        while let Some(batch) = batch_rx.recv().await {
            let frame = HitFrame::Batch {
                seq: batch.seq,
                hits: batch.hits,
            };
            super::write_frame(&mut writer, &frame).await?;
            sent += 1;
        }
        let summary = walk_task
            .await
            .context("join walk task")?
            .context("walk for daemon stream")?;
        debug_assert_eq!(sent, summary.batches, "every walked batch was sent");
        super::write_frame(
            &mut writer,
            &HitFrame::End {
                batches: summary.batches,
            },
        )
        .await?;
        writer.shutdown().await.context("shutdown hit writer")?;
        Ok::<super::engine::WalkSummary, anyhow::Error>(summary)
    });

    // Reader half (current task): read annotation-batches, reassemble into seq
    // order, emit through the sink. The sink is `!Send` (it wraps stdout), so it
    // stays here rather than crossing into a task.
    let read_result = read_and_emit(reader, sink).await;

    // Join the writer so a walk error (bad pattern) or a write fault surfaces
    // even if the read side finished first. A reader fault is the primary error;
    // a writer fault is reported if the reader succeeded.
    let write_result = writer_task.await.context("join writer task")?;
    read_result?;
    let summary = write_result?;
    sink.flush()?;
    Ok(summary)
}

/// Reads annotation-batches from `reader`, reassembles them into `seq` order, and
/// emits each in order through `sink`. Splitting this out keeps the concurrent
/// writer task and the reader loop legible.
async fn read_and_emit<R, Wo>(mut reader: R, sink: &mut ResultSink<Wo>) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    Wo: std::io::Write,
{
    let mut line = String::new();
    let mut reorder = ReorderBuffer::new();
    loop {
        let Some(frame): Option<AnnotationFrame> =
            super::read_frame(&mut reader, &mut line).await?
        else {
            // The daemon closed without a terminator — a stream fault. Degrade.
            return Err(anyhow::anyhow!(
                "annotation stream ended without a terminator"
            ));
        };
        match frame {
            AnnotationFrame::Batch { batch } => {
                for ready in reorder.offer(batch) {
                    emit_batch(&ready, sink)?;
                }
            }
            AnnotationFrame::End { batches } => {
                // Flush any still-buffered batches in order, then stop. A gap
                // here (a batch never arrived) is a stream fault, surfaced.
                let flushed = reorder.drain_in_order();
                for ready in &flushed {
                    emit_batch(ready, sink)?;
                }
                if reorder.emitted != batches {
                    return Err(anyhow::anyhow!(
                        "annotation stream terminator claims {batches} batches, emitted {}",
                        reorder.emitted
                    ));
                }
                return Ok(());
            }
        }
    }
}

/// Emits one annotated batch's hits in order through the result sink.
fn emit_batch<Wo: std::io::Write>(batch: &AnnotatedBatch, sink: &mut ResultSink<Wo>) -> Result<()> {
    for hit in &batch.hits {
        sink.write_line(&hit.render())?;
    }
    Ok(())
}

/// Restores batch-sequence order over annotation-batches that may arrive out of
/// order (the in-flight window lets the daemon resolve a later batch before an
/// earlier slow one).
///
/// Batches are held until the contiguous run starting at `next` is complete; each
/// `offer` returns the batches that just became emittable, in order. This is what
/// makes ordered emission a property of construction rather than of daemon
/// behavior.
struct ReorderBuffer {
    next: u64,
    emitted: u64,
    pending: std::collections::BTreeMap<u64, AnnotatedBatch>,
}

impl ReorderBuffer {
    const fn new() -> Self {
        Self {
            next: 0,
            emitted: 0,
            pending: std::collections::BTreeMap::new(),
        }
    }

    /// Accepts one batch and returns the newly-contiguous run that can now be
    /// emitted, in `seq` order.
    fn offer(&mut self, batch: AnnotatedBatch) -> Vec<AnnotatedBatch> {
        self.pending.insert(batch.seq, batch);
        let mut ready = Vec::new();
        while let Some(batch) = self.pending.remove(&self.next) {
            self.next += 1;
            self.emitted += 1;
            ready.push(batch);
        }
        ready
    }

    /// Drains any batches that form a contiguous run from `next` — used at the
    /// terminator to flush a tail that arrived before its predecessor's gap
    /// closed. A remaining non-contiguous batch signals a stream gap (the caller
    /// checks the emitted count against the terminator).
    fn drain_in_order(&mut self) -> Vec<AnnotatedBatch> {
        let mut ready = Vec::new();
        while let Some(batch) = self.pending.remove(&self.next) {
            self.next += 1;
            self.emitted += 1;
            ready.push(batch);
        }
        ready
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::cast_possible_truncation,
    reason = "tests use expect for readable assertions and small fixture counts"
)]
mod tests {
    use super::super::WireHit;
    use super::super::frame::{AnnotatedHit, AnnotationVerdict};
    use super::*;
    use std::path::PathBuf;

    fn hit(path: &str, line: u32) -> WireHit {
        WireHit {
            path: PathBuf::from(path),
            line,
            column: 1,
            text: format!("line {line}"),
        }
    }

    fn annotated(seq: u64, path: &str) -> AnnotatedBatch {
        AnnotatedBatch {
            seq,
            hits: vec![AnnotatedHit::passthrough(hit(path, seq as u32))],
            verdict: AnnotationVerdict::Annotated,
        }
    }

    #[test]
    fn result_sink_writes_one_atomic_line() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut sink = ResultSink::new(&mut buf);
            sink.write_line("/w/a.rs:1:1:hit").expect("write");
            sink.flush().expect("flush");
        }
        assert_eq!(String::from_utf8(buf).expect("utf8"), "/w/a.rs:1:1:hit\n");
    }

    #[test]
    fn reorder_buffer_restores_seq_order() {
        // Offer batches out of order: 2, 0, 1. Only when 0 arrives does the run
        // open; then 1 (and the buffered 2) flush in order.
        let mut rb = ReorderBuffer::new();
        assert!(
            rb.offer(annotated(2, "/w/c.rs")).is_empty(),
            "2 waits for 0"
        );
        let run0 = rb.offer(annotated(0, "/w/a.rs"));
        assert_eq!(run0.len(), 1, "0 alone becomes ready");
        assert_eq!(run0[0].seq, 0);
        let run1 = rb.offer(annotated(1, "/w/b.rs"));
        // 1 opens 1 and the buffered 2 — both flush, in order.
        let seqs: Vec<u64> = run1.iter().map(|b| b.seq).collect();
        assert_eq!(seqs, vec![1, 2], "1 releases the buffered 2 in order");
        assert_eq!(rb.emitted, 3);
    }

    #[test]
    fn reorder_buffer_in_order_offers_pass_straight_through() {
        let mut rb = ReorderBuffer::new();
        assert_eq!(rb.offer(annotated(0, "/w/a.rs")).len(), 1);
        assert_eq!(rb.offer(annotated(1, "/w/b.rs")).len(), 1);
        assert_eq!(rb.emitted, 2);
    }
}
