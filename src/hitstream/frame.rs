// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The hit-batch frame protocol (ws43): the two frame streams that flow over the
//! existing daemon socket, one in each direction.
//!
//! The CLI owns the walk, so it drives the exchange: it streams [`HitFrame`]
//! frames (an ordered batch of canonical-path hits, plus a terminator) to the
//! daemon, and the daemon streams [`AnnotationFrame`] frames back (the same batch
//! enriched, plus a per-batch **budget verdict**, plus a terminator).
//!
//! Both enums are internally tagged on `"frame"` — the same version-skew hinge
//! the legacy [`crate::router::GrepFrame`] uses. A frame from a peer that speaks
//! a newer protocol carries an unrecognized tag and deserializes to a
//! comprehensible error rather than a silent misparse; the reader treats that as
//! the degrade signal (fall back to the unannotated stream), exactly as a
//! daemon-absent connection would.

use serde::{Deserialize, Serialize};

use super::WireHit;

/// One frame of the CLI → daemon hit stream.
///
/// The CLI emits an ordered sequence of [`HitFrame::Batch`] frames — each a
/// contiguous slice of the walk's hits in the walk's global order, tagged with a
/// monotonic `seq` — terminated by exactly one [`HitFrame::End`]. `seq` lets the
/// daemon's annotation-batches be reassembled into order even when a small
/// in-flight window pipelines them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum HitFrame {
    /// An ordered batch of hits with its batch sequence number.
    Batch {
        /// Monotonic batch sequence number, 0-based, gap-free.
        seq: u64,
        /// The hits in this batch, in the walk's global order. Each path is
        /// canonical (canonicalized at the walk seam).
        hits: Vec<WireHit>,
    },
    /// The terminator: no more batches follow. Carries the total batch count so
    /// the daemon and the CLI agree on how many annotation-batches to expect.
    End {
        /// Total number of [`HitFrame::Batch`] frames sent before this
        /// terminator (`0` for an empty walk).
        batches: u64,
    },
}

/// The per-batch budget verdict the daemon stamps on each annotation-batch.
///
/// The verdict is advisory metadata about *enrichment*, never about hits: an
/// [`AnnotationVerdict::PassedThrough`] batch still carries every hit it was
/// handed, unannotated. This is the degrade-only invariant made explicit on the
/// wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum AnnotationVerdict {
    /// The batch was enriched within budget.
    Annotated,
    /// The batch was returned unannotated because enrichment could not complete
    /// within budget or a dependency was unavailable. `reason` names the cause
    /// (`"budget"`, `"no-server"`, …) so the degrade is never silent.
    PassedThrough {
        /// Why enrichment was skipped for this batch.
        reason: String,
    },
}

impl AnnotationVerdict {
    /// True when the batch was returned unannotated (degraded).
    #[must_use]
    pub const fn is_passed_through(&self) -> bool {
        matches!(self, Self::PassedThrough { .. })
    }
}

/// A single hit as the daemon returns it: the wire hit plus its (optional)
/// enrichment.
///
/// For this ticket the annotator is a pass-through skeleton, so `anchor` is
/// always `None`; the field is the seam the later enrichment migration fills. A
/// `None` anchor renders identically to the CLI's own unannotated spelling, so a
/// pass-through annotation-batch and a daemon-absent batch print the same bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedHit {
    /// The hit, unchanged from the batch the CLI sent.
    #[serde(flatten)]
    pub hit: WireHit,
    /// The enrichment coordinate for this hit, or `None` when the hit was not
    /// enriched (the pass-through skeleton, or a genuinely top-level hit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
}

impl AnnotatedHit {
    /// Wraps a wire hit with no enrichment — the pass-through spelling.
    #[must_use]
    pub const fn passthrough(hit: WireHit) -> Self {
        Self { hit, anchor: None }
    }

    /// Renders this hit as one result line. With no anchor this is byte-identical
    /// to [`WireHit::render_unannotated`], so a pass-through batch and a
    /// daemon-absent batch print the same bytes (the degrade-only invariant).
    #[must_use]
    pub fn render(&self) -> String {
        self.anchor.as_ref().map_or_else(
            || self.hit.render_unannotated(),
            |anchor| {
                format!(
                    "{}:{}:{}#{}:{}",
                    self.hit.path.display(),
                    self.hit.line,
                    self.hit.column,
                    anchor,
                    self.hit.text
                )
            },
        )
    }
}

/// A daemon annotation-batch: the same batch the CLI sent, enriched (or passed
/// through), tagged with its originating `seq` and a budget verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedBatch {
    /// The batch sequence number this annotation-batch answers — echoes the
    /// [`HitFrame::Batch::seq`] it was built from, so the CLI can restore order.
    pub seq: u64,
    /// The enriched (or passed-through) hits, in the batch's original order.
    pub hits: Vec<AnnotatedHit>,
    /// The budget verdict for this batch.
    pub verdict: AnnotationVerdict,
}

/// One frame of the daemon → CLI annotation stream.
///
/// The daemon emits one [`AnnotationFrame::Batch`] per hit-batch it received (in
/// whatever order the in-flight window resolves them — `seq` restores order),
/// terminated by exactly one [`AnnotationFrame::End`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum AnnotationFrame {
    /// One annotated batch.
    Batch {
        /// The annotation-batch payload.
        #[serde(flatten)]
        batch: AnnotatedBatch,
    },
    /// The terminator: no more annotation-batches follow.
    End {
        /// Total number of [`AnnotationFrame::Batch`] frames the daemon emitted.
        batches: u64,
    },
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_hit() -> WireHit {
        WireHit {
            path: PathBuf::from("/w/src/a.rs"),
            line: 3,
            column: 1,
            text: "fn f() {".to_string(),
        }
    }

    #[test]
    fn hit_frame_batch_roundtrips_and_carries_tag() {
        let frame = HitFrame::Batch {
            seq: 7,
            hits: vec![sample_hit()],
        };
        let line = serde_json::to_string(&frame).expect("serialize batch");
        assert!(
            line.contains("\"frame\":\"batch\""),
            "batch carries the frame tag: {line}"
        );
        let back: HitFrame = serde_json::from_str(&line).expect("parse batch");
        assert_eq!(back, frame, "hit batch roundtrips");
    }

    #[test]
    fn hit_frame_end_roundtrips() {
        let frame = HitFrame::End { batches: 3 };
        let line = serde_json::to_string(&frame).expect("serialize end");
        assert!(line.contains("\"frame\":\"end\""), "end carries the tag");
        let back: HitFrame = serde_json::from_str(&line).expect("parse end");
        assert_eq!(back, frame, "hit end roundtrips");
    }

    #[test]
    fn annotation_frame_batch_roundtrips_with_verdict() {
        let frame = AnnotationFrame::Batch {
            batch: AnnotatedBatch {
                seq: 2,
                hits: vec![AnnotatedHit::passthrough(sample_hit())],
                verdict: AnnotationVerdict::PassedThrough {
                    reason: "budget".to_string(),
                },
            },
        };
        let line = serde_json::to_string(&frame).expect("serialize annotated");
        assert!(
            line.contains("\"frame\":\"batch\""),
            "annotation batch carries the frame tag"
        );
        assert!(
            line.contains("\"verdict\":\"passed_through\""),
            "verdict is on the wire: {line}"
        );
        let back: AnnotationFrame = serde_json::from_str(&line).expect("parse annotated");
        assert_eq!(back, frame, "annotation batch roundtrips");
    }

    #[test]
    fn annotated_verdict_annotated_roundtrips() {
        let v = AnnotationVerdict::Annotated;
        let line = serde_json::to_string(&v).expect("serialize verdict");
        assert!(line.contains("\"verdict\":\"annotated\""));
        let back: AnnotationVerdict = serde_json::from_str(&line).expect("parse verdict");
        assert_eq!(back, v);
        assert!(!back.is_passed_through());
    }

    #[test]
    fn unknown_hit_frame_kind_is_a_comprehensible_error() {
        // A newer CLI's unknown frame kind fails to parse — honest degradation.
        assert!(
            serde_json::from_str::<HitFrame>(r#"{"frame":"future_kind","seq":0}"#).is_err(),
            "an unrecognized hit frame kind errors, never misparses",
        );
    }

    #[test]
    fn unknown_annotation_frame_kind_is_a_comprehensible_error() {
        assert!(
            serde_json::from_str::<AnnotationFrame>(r#"{"frame":"future_kind"}"#).is_err(),
            "an unrecognized annotation frame kind errors, never misparses",
        );
    }

    #[test]
    fn passthrough_hit_renders_like_unannotated() {
        // The degrade-only invariant on the render path: a None-anchor annotated
        // hit prints byte-for-byte the CLI's own unannotated spelling.
        let hit = sample_hit();
        let annotated = AnnotatedHit::passthrough(hit.clone());
        assert_eq!(annotated.render(), hit.render_unannotated());
    }

    #[test]
    fn annotated_hit_with_anchor_renders_scope() {
        let annotated = AnnotatedHit {
            hit: sample_hit(),
            anchor: Some("mod_a/f".to_string()),
        };
        assert_eq!(annotated.render(), "/w/src/a.rs:3:1#mod_a/f:fn f() {");
    }
}
