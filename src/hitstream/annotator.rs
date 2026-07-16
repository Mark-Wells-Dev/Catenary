// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The daemon-side annotator loop (ws43).
//!
//! The daemon stops being a query executor and becomes a bounded enrichment
//! annotator on the streamed hit protocol. Its shape — the structure that
//! matters — is: **read batch → await (budgeted) → write batch**, a native async
//! citizen. The budget is a timeout on the awaited enrichment future (the
//! generalized [`crate::lsp::manager::QUERY_ENRICHMENT_BUDGET`] pattern); a blown
//! budget yields a pass-through verdict on a complete, unannotated batch, never a
//! dropped hit.
//!
//! Since ws43-02 the production enricher is real: the grep executor's LSP
//! enrichment lives in [`crate::bridge::GrepHitEnricher`], and the router's
//! `tool/hitstream` arm serves it (wrapped with the query auto-mount). The
//! [`PassThroughEnricher`] remains as the degrade spelling and the protocol
//! tests' stub. What is load-bearing here is the async loop, the budget seam,
//! the bounded in-flight window for pipelining with ordered emission, and the
//! law: **no await while holding a lock guard.**
//!
//! An old daemon that predates this protocol never reaches this loop — it answers
//! the unknown method the same way it answers any unknown request, and the CLI's
//! read of an unrecognized/absent frame lands it on the same fallback as
//! daemon-absent (degrade-only).

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use super::ANNOTATION_BATCH_BUDGET;
use super::frame::{AnnotatedBatch, AnnotatedHit, AnnotationFrame, AnnotationVerdict, HitFrame};

/// The enrichment step the annotator awaits per batch, under budget.
///
/// A production enricher resolves each hit's scope anchor against the LSP graph;
/// the pass-through skeleton ([`PassThroughEnricher`]) resolves nothing. The trait
/// is the seam the later migration fills. Enrichment is `async` and may await
/// LSP round-trips — but it must NEVER hold a lock guard across an await (the
/// ruled law); an enricher that needs shared state clones or scopes the guard so
/// no guard is live at a suspension point.
pub trait BatchEnricher: Send + Sync {
    /// Enriches one batch's hits, returning the annotated hits in the same order.
    ///
    /// This future is what the budget times out. A pass-through implementation
    /// returns immediately; a real one awaits enrichment and returns whatever it
    /// resolved within the time it was given (the caller applies the budget).
    fn enrich(
        &self,
        hits: Vec<super::WireHit>,
    ) -> impl Future<Output = Result<Vec<AnnotatedHit>>> + Send;
}

/// The pass-through enricher: wraps every hit with no anchor, immediately.
///
/// The skeleton's enricher for this ticket. Its batches always render identically
/// to the CLI's own unannotated spelling, so a pass-through annotation stream and
/// a daemon-absent stream print the same bytes (degrade-only).
#[derive(Debug, Default, Clone, Copy)]
pub struct PassThroughEnricher;

impl BatchEnricher for PassThroughEnricher {
    async fn enrich(&self, hits: Vec<super::WireHit>) -> Result<Vec<AnnotatedHit>> {
        Ok(hits.into_iter().map(AnnotatedHit::passthrough).collect())
    }
}

/// Applies the per-batch enrichment budget to `enricher`'s future.
///
/// Returns the annotated batch with a verdict:
/// - [`AnnotationVerdict::Annotated`] when enrichment completed within budget;
/// - [`AnnotationVerdict::PassedThrough`] with a `reason` when the budget blew
///   (`"budget"`) or enrichment failed (`"enrich-error"`).
///
/// In every degraded case the returned batch still carries EVERY hit — the
/// budget bounds enrichment, never the hit set. This is the single seam where the
/// budget/verdict discipline lives.
pub async fn annotate_batch<E: BatchEnricher>(
    enricher: &E,
    seq: u64,
    hits: Vec<super::WireHit>,
    budget: Duration,
) -> AnnotatedBatch {
    // Keep an unannotated copy so a blown budget or an enrich error still returns
    // every hit — complete output, no truncation.
    let passthrough: Vec<AnnotatedHit> = hits
        .iter()
        .cloned()
        .map(AnnotatedHit::passthrough)
        .collect();

    match tokio::time::timeout(budget, enricher.enrich(hits)).await {
        Ok(Ok(annotated)) => AnnotatedBatch {
            seq,
            hits: annotated,
            verdict: AnnotationVerdict::Annotated,
        },
        Ok(Err(_)) => AnnotatedBatch {
            seq,
            hits: passthrough,
            verdict: AnnotationVerdict::PassedThrough {
                reason: "enrich-error".to_string(),
            },
        },
        Err(_elapsed) => AnnotatedBatch {
            seq,
            hits: passthrough,
            verdict: AnnotationVerdict::PassedThrough {
                reason: "budget".to_string(),
            },
        },
    }
}

/// Serves one CLI hit-stream connection: read batches, annotate each under
/// budget, write annotation-batches back.
///
/// The daemon-side leg of the exchange, on the existing socket (the caller has
/// already routed the new frame method here). The loop is a native async citizen:
/// read a [`HitFrame::Batch`], await [`annotate_batch`] (budgeted), write an
/// [`AnnotationFrame::Batch`]. It preserves batch order — a batch is written in
/// the order it was read — and stops on the [`HitFrame::End`] terminator, echoing
/// the batch count back in [`AnnotationFrame::End`].
///
/// The in-flight window ([`super::IN_FLIGHT_WINDOW`]) bounds how many enrichments run
/// concurrently, so a burst of batches cannot spawn unbounded work; ordered
/// emission is preserved because each annotation-batch carries its `seq` and the
/// CLI reassembles (see [`super::sink::daemon_stream`]).
///
/// This skeleton drives enrichment sequentially (window of 1 in effect) — enough
/// to prove the read→await→write loop; the concurrent-window fill is a mechanical
/// extension the enrichment-migration ticket lands with real per-hit work.
///
/// # Errors
///
/// Returns an error on a malformed frame (an unknown kind — honest degradation)
/// or a socket read/write failure. The caller treats an error as connection
/// teardown; the CLI, seeing an incomplete stream, degrades to unannotated.
pub async fn annotate_connection<R, W, E>(
    reader: &mut R,
    writer: &mut W,
    enricher: &E,
    budget: Duration,
) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
    E: BatchEnricher,
{
    let mut line = String::new();
    let mut emitted: u64 = 0;

    loop {
        let Some(frame): Option<HitFrame> = super::read_frame(reader, &mut line).await? else {
            // The CLI closed without a terminator — teardown. Nothing to write.
            return Ok(());
        };
        match frame {
            HitFrame::Batch { seq, hits } => {
                // read → await (budgeted) → write. No lock guard is held across
                // this await (the skeleton holds none; the ruled law binds the
                // real enricher too).
                let batch = annotate_batch(enricher, seq, hits, budget).await;
                super::write_frame(writer, &AnnotationFrame::Batch { batch }).await?;
                emitted += 1;
            }
            HitFrame::End { batches } => {
                debug_assert_eq!(emitted, batches, "annotated every batch the CLI sent");
                super::write_frame(writer, &AnnotationFrame::End { batches: emitted })
                    .await
                    .context("write annotation terminator")?;
                writer
                    .shutdown()
                    .await
                    .context("shutdown annotation writer")?;
                return Ok(());
            }
        }
    }
}

/// Serves one connection with the default pass-through enricher and budget.
///
/// Since ws43-02 the router's `tool/hitstream` arm serves the REAL enricher
/// ([`crate::bridge::GrepHitEnricher`], wrapped with the query auto-mount);
/// this pass-through entry point remains for protocol tests and as the
/// reference degrade spelling.
///
/// # Errors
///
/// Returns an error on a malformed frame or a socket fault (see
/// [`annotate_connection`]).
pub async fn serve_passthrough<R, W>(reader: &mut R, writer: &mut W) -> Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    annotate_connection(
        reader,
        writer,
        &PassThroughEnricher,
        ANNOTATION_BATCH_BUDGET,
    )
    .await
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::super::WireHit;
    use super::*;
    use std::path::PathBuf;

    fn hits(n: u32) -> Vec<WireHit> {
        (0..n)
            .map(|i| WireHit {
                path: PathBuf::from(format!("/w/f{i}.rs")),
                line: i + 1,
                column: 1,
                text: format!("hit {i}"),
            })
            .collect()
    }

    /// An enricher that sleeps longer than any budget the test gives it, to
    /// prove the budget blows to a pass-through verdict carrying every hit.
    struct SlowEnricher;
    impl BatchEnricher for SlowEnricher {
        async fn enrich(&self, hits: Vec<WireHit>) -> Result<Vec<AnnotatedHit>> {
            tokio::time::sleep(Duration::from_hours(1)).await;
            Ok(hits.into_iter().map(AnnotatedHit::passthrough).collect())
        }
    }

    /// An enricher that fails, to prove an enrich error degrades (not aborts).
    struct FailingEnricher;
    impl BatchEnricher for FailingEnricher {
        async fn enrich(&self, _hits: Vec<WireHit>) -> Result<Vec<AnnotatedHit>> {
            Err(anyhow::anyhow!("enrichment unavailable"))
        }
    }

    #[tokio::test]
    async fn passthrough_batch_is_annotated_verdict_with_no_anchors() {
        let batch = annotate_batch(&PassThroughEnricher, 0, hits(3), Duration::from_secs(5)).await;
        assert_eq!(batch.seq, 0);
        assert_eq!(batch.hits.len(), 3, "every hit is present");
        assert!(matches!(batch.verdict, AnnotationVerdict::Annotated));
        assert!(batch.hits.iter().all(|h| h.anchor.is_none()));
    }

    /// Extracts the pass-through reason, or `None` when the verdict is
    /// `Annotated` — a test helper that avoids the denied bare `panic!` on the
    /// wrong variant.
    fn passthrough_reason(verdict: &AnnotationVerdict) -> Option<&str> {
        match verdict {
            AnnotationVerdict::PassedThrough { reason } => Some(reason.as_str()),
            AnnotationVerdict::Annotated => None,
        }
    }

    #[tokio::test]
    async fn blown_budget_passes_through_with_every_hit() {
        let batch = annotate_batch(&SlowEnricher, 5, hits(4), Duration::from_millis(20)).await;
        assert_eq!(batch.seq, 5, "seq is preserved through a blown budget");
        assert_eq!(batch.hits.len(), 4, "budget bounds enrichment, never hits");
        assert_eq!(
            passthrough_reason(&batch.verdict),
            Some("budget"),
            "a blown budget passes through with the budget reason"
        );
    }

    #[tokio::test]
    async fn enrich_error_passes_through_with_every_hit() {
        let batch = annotate_batch(&FailingEnricher, 2, hits(2), Duration::from_secs(5)).await;
        assert_eq!(
            batch.hits.len(),
            2,
            "a failed enrich still returns every hit"
        );
        assert_eq!(
            passthrough_reason(&batch.verdict),
            Some("enrich-error"),
            "a failed enrich passes through with the enrich-error reason"
        );
    }

    /// Daemon-side unknown-frame handling: an unrecognized frame from the CLI is
    /// a comprehensible error that tears the connection down — never a hang, never
    /// a silent misparse.
    #[tokio::test]
    async fn unknown_frame_from_cli_is_an_error_not_a_hang() {
        use tokio::io::AsyncWriteExt;

        let (mut cli_writes, daemon_reads) = tokio::io::duplex(4096);
        let (daemon_writes, _cli_reads) = tokio::io::duplex(4096);

        // The CLI (buggy or newer) sends a frame the daemon does not recognize.
        cli_writes
            .write_all(b"{\"frame\":\"future_kind\",\"seq\":0}\n")
            .await
            .expect("write unknown frame");
        cli_writes.shutdown().await.expect("shutdown");

        let mut reader = tokio::io::BufReader::new(daemon_reads);
        let mut writer = daemon_writes;
        let result = annotate_connection(
            &mut reader,
            &mut writer,
            &PassThroughEnricher,
            Duration::from_secs(5),
        )
        .await;
        assert!(
            result.is_err(),
            "an unknown hit frame is a comprehensible error, not a hang or a misparse"
        );
    }
}
