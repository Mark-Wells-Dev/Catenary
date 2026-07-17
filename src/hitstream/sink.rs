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
//! ## Degrade in place (ws43-02)
//!
//! Since the CLI cutover, [`daemon_stream`] never surfaces a mid-exchange stream
//! fault as an error the caller must recover from: it holds every sent batch in a
//! retention buffer until its annotation is emitted, and on ANY annotation-stream
//! fault — a malformed frame (the old-daemon signal), a premature EOF, a
//! terminator gap, or the per-read deadline ([`super::STREAM_READ_DEADLINE`])
//! expiring against an accepts-then-silent daemon — it **completes the stream in
//! place**, emitting the un-annotated remainder in order. Already-emitted lines
//! are never re-emitted and no hit is ever dropped: degrade-only, by
//! construction. Only a walk failure (an uncompilable pattern or filter) is an
//! error.
//!
//! ## Stream discipline, by construction
//!
//! [`ResultSink`] is the ONLY writer of a result line, and it frames each line as
//! a single buffered `write_all`. Advisories go to stderr through
//! [`super::advise`], a physically distinct stream. There is no shared buffer and
//! no partial-line window, so the class of bug where a stderr hint fuses into a
//! stdout result line under piping cannot occur: a result line is one atomic
//! write on one stream, an advisory is one atomic write on the other.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use super::frame::{AnnotatedBatch, AnnotatedHit, AnnotationFrame, HitFrame};
use super::{HitBatch, IN_FLIGHT_WINDOW, STREAM_READ_DEADLINE, WireHit};

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

/// The CLI-side display mapping and grep-line rendering for the streamed engine
/// (ws43-02).
///
/// Display mapping is the CLI's job — the wire carries canonical absolute paths,
/// and this maps each onto today's `catenary grep` display spelling: relative to
/// the (canonicalized) invoking cwd when the hit lies under it, the absolute
/// path otherwise. With no cwd (protocol tests), the absolute path prints.
#[derive(Debug, Clone, Default)]
pub struct GrepRender {
    /// The canonicalized invoking cwd hits are displayed relative to, or `None`
    /// for absolute display.
    cwd: Option<PathBuf>,
}

impl GrepRender {
    /// A renderer displaying hits relative to `cwd` (pass the canonicalized
    /// cwd — hit paths are canonical, so the strip must be
    /// canonical-to-canonical).
    #[must_use]
    pub const fn new(cwd: Option<PathBuf>) -> Self {
        Self { cwd }
    }

    /// The display spelling for one canonical hit path: cwd-relative when under
    /// the cwd, absolute otherwise — exactly the retired executor's `rel_path`
    /// rule for a cwd-scoped query.
    #[must_use]
    pub fn display(&self, path: &Path) -> String {
        self.cwd.as_ref().map_or_else(
            || path.display().to_string(),
            |base| {
                path.strip_prefix(base).map_or_else(
                    |_| path.to_string_lossy().into_owned(),
                    |rel| rel.to_string_lossy().into_owned(),
                )
            },
        )
    }

    /// One annotated result line in the `catenary grep` shape
    /// (`display:line#trail:text` / `display:line:text` / `display:line#?:text`).
    #[must_use]
    pub fn annotated_line(&self, hit: &AnnotatedHit) -> String {
        hit.render_grep_line(&self.display(&hit.hit.path))
    }

    /// One un-annotated result line in the grep degrade shape
    /// (`display:line#?:text`) — byte-identical to a pass-through annotated hit.
    #[must_use]
    pub fn unannotated_line(&self, hit: &WireHit) -> String {
        hit.render_grep_unannotated(&self.display(&hit.path))
    }
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
/// to `sink`, unannotated, in the `catenary grep` degrade shape
/// (`display:line#?:text`).
///
/// Runs the walk with a streaming callback that renders each hit through
/// `render` and writes it via the [`ResultSink`] — no daemon, no buffered result
/// set. The bytes are identical to a pass-through annotation-batch (a
/// `false`-enriched hit renders the same spelling), which is exactly what makes
/// daemon-absent, wedged-daemon, and old-daemon degrade to the same stream.
///
/// Returns the walk's [`WalkSummary`](super::engine::WalkSummary) so the caller
/// can report skips (misc 135) on stderr.
///
/// # Errors
///
/// Returns an error if the walk fails to start (bad pattern) or a write fails.
pub fn stdout_unannotated<W: std::io::Write>(
    pattern: &str,
    roots: &[std::path::PathBuf],
    options: &super::engine::WalkOptions,
    render: &GrepRender,
    sink: &mut ResultSink<W>,
) -> Result<super::engine::WalkSummary> {
    let summary = super::engine::walk(pattern, roots, options, |batch: HitBatch| {
        for hit in &batch.hits {
            sink.write_line(&render.unannotated_line(hit))?;
        }
        Ok(())
    })?;
    sink.flush()?;
    Ok(summary)
}

/// What one [`daemon_stream`] run did: the walk's summary, plus whether the
/// annotation stream degraded mid-exchange (the caller's cue for a one-line
/// stderr advisory — results are complete either way).
pub struct DaemonStreamReport {
    /// The walk summary (batch count, skips). `observed` has been taken — it
    /// was shipped on the [`HitFrame::End`] terminator.
    pub summary: super::engine::WalkSummary,
    /// True when the annotation stream faulted or stalled and the remainder of
    /// the results were emitted unannotated in place. Never fewer results.
    pub degraded: bool,
}

/// The daemon-stream sink: send hit-batches to the daemon, read annotation-batches
/// back, and emit them **in batch-sequence order** through `sink`, in the
/// `catenary grep` line shape.
///
/// The exchange is genuinely pipelined: the writer half (a spawned task) streams
/// hit-batches to the daemon while the emitter — the current task — drains
/// annotation-batches and emits. The two run concurrently, so neither side blocks
/// waiting for the other to finish. The in-flight window ([`IN_FLIGHT_WINDOW`])
/// is the channel capacity that feeds the writer, so the walk cannot race
/// arbitrarily far ahead and buffer a whole result set (the streaming,
/// no-buffered-result-set invariant; the retention buffer below holds only the
/// sent-but-not-yet-annotated window, bounded in practice by the socket buffer).
///
/// Because the daemon may resolve batches out of order (a slow batch finishes
/// after a later fast one), the emitter **reassembles** annotation-batches into
/// `seq` order before writing a line — a slow batch delays emission but never
/// reorders it.
///
/// Every sent batch is retained until its annotation is emitted. On any
/// annotation-stream fault — a malformed frame (old daemon), premature EOF, a
/// terminator gap, or no frame within [`STREAM_READ_DEADLINE`] while an
/// annotation is outstanding (an accepts-then-silent daemon) — the emitter
/// switches to the degrade path in place: the retained batches and the rest of
/// the walk emit unannotated, in order, with nothing duplicated and nothing
/// dropped. `reap_scopes` rides the [`HitFrame::End`] terminator with the walk's
/// observation set (ws43-02 reap parity); a zero-match walk ships neither
/// (executor parity — a query with no matches never nudged).
///
/// # Errors
///
/// Returns an error only if the walk itself fails (bad pattern or filter) or a
/// result-sink write fails. Stream faults are not errors — they degrade.
#[allow(
    clippy::too_many_arguments,
    reason = "the walk inputs plus the split connection plus the two output seams"
)]
#[allow(
    clippy::similar_names,
    reason = "`reader` and `render` are the two distinct seams this function joins"
)]
pub async fn daemon_stream<R, Wr, Wo>(
    pattern: &str,
    roots: &[std::path::PathBuf],
    options: &super::engine::WalkOptions,
    reap_scopes: Option<Vec<PathBuf>>,
    reader: R,
    writer: Wr,
    render: &GrepRender,
    sink: &mut ResultSink<Wo>,
) -> Result<DaemonStreamReport>
where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
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

    // Degrade latch, shared with the writer: once set, no further byte is
    // written to the daemon (a wedged peer must not block the degrade drain).
    let degraded_flag = Arc::new(AtomicBool::new(false));

    // Retention: every batch, in seq order, for the emitter — it holds each
    // until its annotation is emitted, and emits the remainder unannotated on a
    // fault. Unbounded by type, bounded in practice: the writer's socket write
    // backpressures the bounded walk channel, so retention holds only the
    // in-flight window the daemon has accepted but not yet annotated.
    let (retain_tx, mut retain_rx) = mpsc::unbounded_channel::<HitBatch>();

    // Writer half runs CONCURRENTLY with the emitter below, so the daemon can
    // drain hit-frames and answer annotation-frames while the CLI is still
    // walking — the pipeline. Every socket write is bounded and degrade-aware:
    // a write fault or a stalled peer flips the latch and the writer keeps
    // draining the walk into retention only.
    let writer_degraded = Arc::clone(&degraded_flag);
    let writer_task = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut writer = writer;
        while let Some(mut batch) = batch_rx.recv().await {
            let frame_batch = HitFrame::Batch {
                seq: batch.seq,
                hits: batch.hits.clone(),
                // Observations ride the wire only — the retention copy exists
                // to re-emit HITS unannotated on a degrade, and a degraded
                // stream nudges nothing.
                observed: std::mem::take(&mut batch.observed),
            };
            // Retention next: the emitter owns this copy from here on.
            let _ = retain_tx.send(batch);
            if !writer_degraded.load(Ordering::Acquire) {
                let bounded = tokio::time::timeout(
                    STREAM_READ_DEADLINE,
                    super::write_frame(&mut writer, &frame_batch),
                )
                .await;
                if !matches!(bounded, Ok(Ok(()))) {
                    // Write fault or a peer that stopped reading: degrade.
                    // Retention already carries the batch; nothing is lost.
                    writer_degraded.store(true, Ordering::Release);
                }
            }
        }
        let mut summary = walk_task
            .await
            .context("join walk task")?
            .context("walk for daemon stream")?;
        // Closing retention tells the emitter the walk is complete (and that
        // the terminator — if we are still healthy — is on the wire next).
        drop(retain_tx);
        if !writer_degraded.load(Ordering::Acquire) {
            // Reap parity (ws43-02): the observation set and the pathless-walk
            // reap scopes ride the terminator. A zero-match walk ships neither —
            // executor parity: a query with no matches never nudged.
            let (observed, reap_scopes) = if summary.batches == 0 {
                (Vec::new(), None)
            } else {
                (std::mem::take(&mut summary.observed), reap_scopes)
            };
            let end = HitFrame::End {
                batches: summary.batches,
                observed,
                reap_scopes,
            };
            let bounded =
                tokio::time::timeout(STREAM_READ_DEADLINE, super::write_frame(&mut writer, &end))
                    .await;
            if matches!(bounded, Ok(Ok(()))) {
                let _ = writer.shutdown().await;
            } else {
                writer_degraded.store(true, Ordering::Release);
            }
        }
        Ok::<super::engine::WalkSummary, anyhow::Error>(summary)
    });

    // Annotation reader: its own task, so the emitter's select never cancels a
    // partial frame read (read_line is not cancellation-safe). A parse fault or
    // EOF simply closes the channel — the emitter reads that as the degrade
    // signal.
    let (ann_tx, mut ann_rx) = mpsc::channel::<AnnotationFrame>(IN_FLIGHT_WINDOW);
    let read_task = tokio::spawn(async move {
        let mut reader = reader;
        let mut line = String::new();
        loop {
            match super::read_frame::<_, AnnotationFrame>(&mut reader, &mut line).await {
                Ok(Some(frame)) => {
                    let done = matches!(frame, AnnotationFrame::End { .. });
                    if ann_tx.send(frame).await.is_err() || done {
                        return;
                    }
                }
                // Clean EOF or a malformed/unknown frame (the old-daemon
                // signal): close the channel; the emitter degrades.
                Ok(None) | Err(_) => return,
            }
        }
    });

    // Emitter (current task — the sink is `!Send`): reassemble annotations into
    // seq order, emit, prune retention; degrade in place on any fault.
    let emit_result = emit_stream(&mut retain_rx, &mut ann_rx, render, sink).await;

    // Shutdown discipline: stop the reader (it may be blocked on a socket that
    // will never speak again), then join the writer so a walk error surfaces.
    read_task.abort();
    let _ = read_task.await;
    if emit_result.as_ref().map_or(true, |degraded| *degraded) {
        degraded_flag.store(true, Ordering::Release);
    }
    let summary = writer_task.await.context("join writer task")??;
    let degraded = emit_result?;
    sink.flush()?;
    Ok(DaemonStreamReport { summary, degraded })
}

/// The emitter loop behind [`daemon_stream`]: ordered annotated emission while
/// the stream is healthy, in-place unannotated completion once it is not.
///
/// Returns whether the stream degraded. Errors only on a result-sink write
/// failure.
async fn emit_stream<Wo: std::io::Write>(
    retain_rx: &mut tokio::sync::mpsc::UnboundedReceiver<HitBatch>,
    ann_rx: &mut tokio::sync::mpsc::Receiver<AnnotationFrame>,
    render: &GrepRender,
    sink: &mut ResultSink<Wo>,
) -> Result<bool> {
    let mut retained: VecDeque<HitBatch> = VecDeque::new();
    let mut retention_open = true;
    let mut reorder = ReorderBuffer::new();
    // The per-read deadline clock: reset whenever an annotation arrives or the
    // outstanding set transitions from empty. Armed only while an annotation is
    // actually outstanding, so a long quiet walk (nothing sent, nothing owed)
    // never trips it.
    let mut wait_since = tokio::time::Instant::now();
    let mut ann_done = false;

    // Healthy loop: any `break` is the degrade signal; the one success exit
    // returns directly.
    'healthy: loop {
        // Prune retention: everything below the reorder watermark has been
        // emitted annotated.
        while retained
            .front()
            .is_some_and(|batch| batch.seq < reorder.next)
        {
            retained.pop_front();
        }
        if ann_done {
            // Every retention send (and the close) was enqueued before our
            // terminator went on the wire, so before the daemon's terminator
            // could exist — drain the queue non-blocking to observe it.
            while retention_open {
                match retain_rx.try_recv() {
                    Ok(batch) => {
                        if batch.seq >= reorder.next {
                            retained.push_back(batch);
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        retention_open = false;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                }
            }
            while retained
                .front()
                .is_some_and(|batch| batch.seq < reorder.next)
            {
                retained.pop_front();
            }
            if retention_open || !retained.is_empty() {
                // The daemon terminated before our walk finished, or a batch
                // was never annotated despite a matching count — degrade the
                // remainder.
                break 'healthy;
            }
            return Ok(false);
        }
        // An annotation is outstanding when a sent batch awaits its
        // annotation, or the walk (and terminator) are done and the daemon's
        // terminator is still owed.
        let outstanding = !retained.is_empty() || !retention_open;
        tokio::select! {
            biased;
            frame = ann_rx.recv() => match frame {
                Some(AnnotationFrame::Batch { batch }) => {
                    wait_since = tokio::time::Instant::now();
                    for ready in reorder.offer(batch) {
                        emit_annotated(&ready, render, sink)?;
                    }
                }
                Some(AnnotationFrame::End { batches }) => {
                    for ready in reorder.drain_in_order() {
                        emit_annotated(&ready, render, sink)?;
                    }
                    if reorder.emitted == batches {
                        ann_done = true;
                    } else {
                        // A gap the terminator cannot paper over.
                        break 'healthy;
                    }
                }
                // Fault or EOF before the terminator (the old-daemon reply, a
                // died daemon, a malformed frame): degrade.
                None => break 'healthy,
            },
            batch = retain_rx.recv(), if retention_open => if let Some(batch) = batch {
                if batch.seq >= reorder.next {
                    if retained.is_empty() {
                        wait_since = tokio::time::Instant::now();
                    }
                    retained.push_back(batch);
                }
            } else {
                retention_open = false;
                // The terminator is on the wire — the daemon now owes its
                // own; start the clock for that final wait.
                wait_since = tokio::time::Instant::now();
            },
            () = tokio::time::sleep_until(wait_since + STREAM_READ_DEADLINE), if outstanding => {
                // An accepts-then-silent daemon: complete unannotated.
                break 'healthy;
            }
        }
    }

    // Degrade drain: emit everything not yet emitted, in seq order — the
    // retained window first, then the rest of the walk as it streams in.
    // Nothing re-emits (the watermark guards) and nothing is dropped.
    while let Some(batch) = retained.pop_front() {
        if batch.seq >= reorder.next {
            emit_unannotated(&batch, render, sink)?;
        }
    }
    while retention_open {
        if let Some(batch) = retain_rx.recv().await {
            if batch.seq >= reorder.next {
                emit_unannotated(&batch, render, sink)?;
            }
        } else {
            retention_open = false;
        }
    }
    Ok(true)
}

/// Emits one annotated batch's hits in order through the result sink.
fn emit_annotated<Wo: std::io::Write>(
    batch: &AnnotatedBatch,
    render: &GrepRender,
    sink: &mut ResultSink<Wo>,
) -> Result<()> {
    for hit in &batch.hits {
        sink.write_line(&render.annotated_line(hit))?;
    }
    Ok(())
}

/// Emits one retained batch's hits unannotated (the degrade spelling).
fn emit_unannotated<Wo: std::io::Write>(
    batch: &HitBatch,
    render: &GrepRender,
    sink: &mut ResultSink<Wo>,
) -> Result<()> {
    for hit in &batch.hits {
        sink.write_line(&render.unannotated_line(hit))?;
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

    #[test]
    fn grep_render_maps_display_paths() {
        let render = GrepRender::new(Some(PathBuf::from("/w")));
        assert_eq!(render.display(Path::new("/w/src/a.rs")), "src/a.rs");
        assert_eq!(
            render.display(Path::new("/elsewhere/b.rs")),
            "/elsewhere/b.rs"
        );
        let absolute = GrepRender::default();
        assert_eq!(absolute.display(Path::new("/w/src/a.rs")), "/w/src/a.rs");
    }

    #[test]
    fn grep_render_lines_match_the_grep_shape() {
        let render = GrepRender::new(Some(PathBuf::from("/w")));
        let wire = hit("/w/src/a.rs", 3);
        assert_eq!(render.unannotated_line(&wire), "src/a.rs:3#?:line 3");
        let scoped = AnnotatedHit {
            hit: wire.clone(),
            anchor: Some("mod/f".to_string()),
            enriched: true,
        };
        assert_eq!(render.annotated_line(&scoped), "src/a.rs:3#mod/f:line 3");
        assert_eq!(
            render.annotated_line(&AnnotatedHit::passthrough(wire.clone())),
            render.unannotated_line(&wire),
            "pass-through and degrade spell the same bytes"
        );
    }
}
