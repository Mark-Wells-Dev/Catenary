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
//! the retired chunked `tool/grep` framing used. A frame from a peer that speaks
//! a newer protocol carries an unrecognized tag and deserializes to a
//! comprehensible error rather than a silent misparse; the reader treats that as
//! the degrade signal (complete the stream unannotated), exactly as a
//! daemon-absent connection would.

use serde::{Deserialize, Serialize};

use super::WireHit;

/// The enrichment weight a hit batch requests (ws43-03).
///
/// Computed **CLI-side** from the query shape and carried on each
/// [`HitFrame::Batch`]: absent for a grep batch (anchor enrichment — the
/// pre-weight wire, byte-identical), present for a glob batch (outline
/// enrichment at the named weight). The annotator honors the requested weight;
/// the per-batch budget still bounds everything. Version skew degrades
/// gracefully in both directions: an old daemon ignoring the field
/// anchor-enriches (over-enrichment the CLI reads as outline-less and renders
/// unenriched — never fewer paths), and an old CLI sends no weight, getting
/// exactly the grep behavior it always got.
///
/// There is deliberately **no enrichment-off weight** (`--paths` was rejected
/// by ruling): a first-class off-switch becomes the taught habit and the
/// product dies by opt-out. `--count` covers tallies; a pipeline needing bare
/// paths strips indented enrichment downstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnrichmentWeight {
    /// Listing weight — the ruled default for listing shapes: each file's
    /// **top-level** symbols only, no nested tree. The finding this ruled
    /// lever answers: 45–360KB of full symbol outlines for plain directory
    /// listings, reproduced five times by five different agents.
    Listing,
    /// The full picture: the fully-expanded types-and-callables outline tree.
    /// `--outline` opts up to it on demand; it stays the default for the
    /// single-file outline shape (`catenary glob src/main.rs`).
    Outline,
}

/// The walk's enrichment tier (brackets 04): declared by the command, decided
/// by the walk's **anchor** — never guessed from the pattern or the hits.
///
/// Computed **CLI-side** at planning time, before any walk I/O
/// ([`crate::bridge::resolve_walk_tier`]): the walk's anchor directory (the cwd
/// of a pathless grep, a path argument's metachar-free base, glob's pattern
/// base) is resolved to its enclosing root — a repository-marker root, or a
/// root the daemon already serves. Anchored **inside** a root
/// → [`WalkTier::Dig`]: project-grade enrichment from that root's instances,
/// exactly the pre-tier behavior (nested roots under the anchor ride along).
/// Anchored **above** every root → [`WalkTier::Sweep`], by its own
/// declaration: file-grade enrichment only, served through the rootless
/// single-file singletons — no project loads, no mounts, no per-hit-root
/// spawns. One decision per walk; there are no mid-stream flips, and a sweep
/// whose hits all land in one root still gets file-grade (conscious
/// conservatism — the query asked for a sweep).
///
/// Version skew degrades to the pre-tier behavior in both directions: an old
/// CLI sends no tier and parses as `Dig` (today's wire, byte-identical), and
/// an old daemon ignores the field and serves the dig path (project-grade — an
/// over-enrichment, never fewer results).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WalkTier {
    /// Anchored inside a project root: project-grade enrichment from the
    /// root's instances — the pre-tier behavior, and the wire default.
    #[default]
    Dig,
    /// Anchored above every project root: file-grade enrichment only, through
    /// the rootless single-file singletons.
    Sweep,
}

impl WalkTier {
    /// Whether this is the dig tier (the wire default — used as the
    /// `skip_serializing_if` predicate so a dig batch serializes byte-identically
    /// to the pre-tier wire).
    #[must_use]
    #[allow(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde skip_serializing_if requires a &T predicate"
    )]
    pub const fn is_dig(&self) -> bool {
        matches!(self, Self::Dig)
    }

    /// Whether this is the sweep tier (file-grade enrichment only).
    #[must_use]
    pub const fn is_sweep(self) -> bool {
        matches!(self, Self::Sweep)
    }
}

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
        /// The requested enrichment weight (ws43-03): `None` for a grep batch
        /// (anchor enrichment — serializes byte-identically to the pre-weight
        /// wire), `Some` for a glob batch (outline enrichment at the named
        /// weight). Unknown-field tolerance carries the skew: an old daemon
        /// ignores it and over-enriches (anchors the CLI reads as
        /// outline-less), which degrades gracefully.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        weight: Option<EnrichmentWeight>,
        /// The walk's anchor-decided enrichment tier (brackets 04). `Dig` — the
        /// default, and what an old CLI's field-less batch parses as —
        /// serializes byte-identically to the pre-tier wire; `Sweep` selects
        /// file-grade enrichment through the rootless singletons. Unknown-field
        /// tolerance carries the skew: an old daemon ignores it and serves the
        /// dig path (over-enrichment, never fewer results).
        #[serde(default, skip_serializing_if = "WalkTier::is_dig")]
        tier: WalkTier,
    },
    /// The terminator: no more batches follow. Carries the total batch count so
    /// the daemon and the CLI agree on how many annotation-batches to expect.
    ///
    /// It carries nothing else (bug 146's final contraction). The terminator
    /// used to ship the walk's observation set and the reap scopes a pathless
    /// walk claimed; both retired with the search walk's observation channel —
    /// a walk's yield was never its coverage, and a filter turned that
    /// mismatch into phantom deletions. An older CLI's `observed` /
    /// `reap_scopes` fields are unknown fields here and are ignored, so skew
    /// degrades to exactly today's behavior: the daemon observes for itself.
    End {
        /// Total number of [`HitFrame::Batch`] frames sent before this
        /// terminator (`0` for an empty walk).
        batches: u64,
    },
}

impl HitFrame {
    /// A plain batch with no weight and the dig tier — the shape an old CLI
    /// sends (and the protocol tests' spelling).
    #[must_use]
    pub const fn batch(seq: u64, hits: Vec<WireHit>) -> Self {
        Self::Batch {
            seq,
            hits,
            weight: None,
            tier: WalkTier::Dig,
        }
    }

    /// The terminator for `batches` batches.
    #[must_use]
    pub const fn end(batches: u64) -> Self {
        Self::End { batches }
    }
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
/// The anchor state is tri-valued, mirroring the grep executor's `Anchor`
/// (ws43-02, the enrichment migration):
///
/// - `anchor: Some(trail)` — enriched, inside a scope (`#trail`);
/// - `anchor: None, enriched: true` — enriched, genuinely top-level (no `#` at
///   all — there is no graph coordinate to report);
/// - `anchor: None, enriched: false` — could not be enriched (no covering
///   server, blown budget, pass-through) — the `#?` marker, so degradation is
///   never misread as top-level.
///
/// `enriched` is a serde-default `false` field, so a frame from a peer that
/// predates it parses as could-not-enrich — the honest degrade reading.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotatedHit {
    /// The hit, unchanged from the batch the CLI sent.
    #[serde(flatten)]
    pub hit: WireHit,
    /// The `#scope` containment trail for this hit, or `None` when the hit has
    /// no scope coordinate (top-level, or not enriched — `enriched` splits the
    /// two).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// Whether enrichment actually covered this hit's file. `false` for a
    /// pass-through hit (budget, no server, degrade), which renders the `#?`
    /// could-not-enrich marker in the grep line shape. For a glob (weighted)
    /// hit, `false` renders the same `no outline` marker an uncovered file
    /// always carried — degradation is never misread as "empty file".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub enriched: bool,
    /// The file's rendered outline body at the batch's requested weight
    /// (ws43-03, glob batches only): one node per line
    /// (`<line>  <declaration source>`), nested nodes tab-indented, **no base
    /// indent** — the CLI re-indents each line for its listing position. No
    /// trailing newline. `None` for a grep hit, for a covered file with no
    /// symbols (`enriched` splits that from could-not-enrich), and for a
    /// suppressed outline. Absent from an old daemon's frames — the CLI reads
    /// that as outline-less and renders the listing unenriched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outline: Option<String>,
    /// The file has symbols but its outline is suppressed by config
    /// (`[tools.glob] outline_suppress`) — the CLI renders the
    /// `[symbols available]` flag in place of the body. Serde-default `false`,
    /// so old-daemon frames parse honestly.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub suppressed: bool,
}

impl AnnotatedHit {
    /// Wraps a wire hit with no enrichment — the pass-through spelling
    /// (`enriched: false`, rendering the `#?` marker in the grep line shape).
    #[must_use]
    pub const fn passthrough(hit: WireHit) -> Self {
        Self {
            hit,
            anchor: None,
            enriched: false,
            outline: None,
            suppressed: false,
        }
    }

    /// An enriched hit inside a scope — the `#trail` spelling.
    #[must_use]
    pub const fn scoped(hit: WireHit, trail: String) -> Self {
        Self {
            hit,
            anchor: Some(trail),
            enriched: true,
            outline: None,
            suppressed: false,
        }
    }

    /// An enriched, genuinely top-level hit (no graph coordinate to report).
    #[must_use]
    pub const fn top_level(hit: WireHit) -> Self {
        Self {
            hit,
            anchor: None,
            enriched: true,
            outline: None,
            suppressed: false,
        }
    }

    /// Renders this hit as one result line in the protocol skeleton's
    /// wire-debug spelling (`path:line:column…`). With no anchor this is
    /// byte-identical to [`WireHit::render_unannotated`], so a pass-through
    /// batch and a daemon-absent batch print the same bytes (the degrade-only
    /// invariant). The user-visible `catenary grep` shape is
    /// [`Self::render_grep_line`] — what the sinks emit since the ws43-02
    /// cutover; this spelling remains for protocol debugging.
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

    /// Renders this hit as one `catenary grep` result line — today's CLI output
    /// shape, byte-compatible with the grep executor's `render_hit_line`:
    ///
    /// - scoped: `display:line#trail:text`
    /// - top-level: `display:line:text`
    /// - could not enrich: `display:line#?:text`
    ///
    /// `display_path` is the CLI-side display spelling (cwd-relative, or the
    /// absolute fallback) — display mapping is the CLI's job, so the canonical
    /// wire path never prints directly. A pass-through hit renders identically
    /// to [`WireHit::render_grep_unannotated`], which is what keeps the degrade
    /// matrix byte-identical on results.
    #[must_use]
    pub fn render_grep_line(&self, display_path: &str) -> String {
        match (&self.anchor, self.enriched) {
            (Some(trail), _) => {
                format!("{display_path}:{}#{trail}:{}", self.hit.line, self.hit.text)
            }
            (None, true) => format!("{display_path}:{}:{}", self.hit.line, self.hit.text),
            (None, false) => format!("{display_path}:{}#?:{}", self.hit.line, self.hit.text),
        }
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
        let frame = HitFrame::batch(7, vec![sample_hit()]);
        let line = serde_json::to_string(&frame).expect("serialize batch");
        assert!(
            line.contains("\"frame\":\"batch\""),
            "batch carries the frame tag: {line}"
        );
        assert!(
            !line.contains("observed") && !line.contains("weight") && !line.contains("tier"),
            "an observation-less, weight-less, dig-tier batch serializes exactly \
             as before (old-daemon compatibility): {line}"
        );
        let back: HitFrame = serde_json::from_str(&line).expect("parse batch");
        assert_eq!(back, frame, "hit batch roundtrips");
    }

    #[test]
    fn hit_frame_batch_weight_roundtrips_and_defaults_none() {
        // The ws43-03 weight lever on the wire: a glob batch names its weight;
        // a weight-less (grep / old-CLI) batch parses as `None`.
        let frame = HitFrame::Batch {
            seq: 0,
            hits: vec![sample_hit()],
            weight: Some(EnrichmentWeight::Listing),
            tier: WalkTier::Dig,
        };
        let line = serde_json::to_string(&frame).expect("serialize batch");
        assert!(
            line.contains("\"weight\":\"listing\""),
            "the requested weight rides the batch frame: {line}"
        );
        let back: HitFrame = serde_json::from_str(&line).expect("parse batch");
        assert_eq!(back, frame, "weighted batch roundtrips");

        let full = HitFrame::Batch {
            seq: 1,
            hits: Vec::new(),
            weight: Some(EnrichmentWeight::Outline),
            tier: WalkTier::Dig,
        };
        let line = serde_json::to_string(&full).expect("serialize batch");
        assert!(
            line.contains("\"weight\":\"outline\""),
            "--outline opts up on the wire: {line}"
        );

        // A field-less batch (old CLI, or grep) reads as weight-less.
        let legacy: HitFrame = serde_json::from_str(
            r#"{"frame":"batch","seq":1,"hits":[{"path":"/w/src/a.rs","line":3,"column":1,"text":"fn f() {"}]}"#,
        )
        .expect("parse legacy batch");
        assert!(
            matches!(legacy, HitFrame::Batch { weight: None, .. }),
            "absent weight reads as None (anchor enrichment)"
        );
    }

    #[test]
    fn hit_frame_batch_tier_roundtrips_and_defaults_dig() {
        // The brackets-04 tier lever on the wire: a sweep batch names its tier;
        // a tier-less (dig / old-CLI) batch parses as `Dig`.
        let frame = HitFrame::Batch {
            seq: 0,
            hits: vec![sample_hit()],
            weight: None,
            tier: WalkTier::Sweep,
        };
        let line = serde_json::to_string(&frame).expect("serialize batch");
        assert!(
            line.contains("\"tier\":\"sweep\""),
            "the sweep declaration rides the batch frame: {line}"
        );
        let back: HitFrame = serde_json::from_str(&line).expect("parse batch");
        assert_eq!(back, frame, "sweep batch roundtrips");

        // A field-less batch (old CLI, or an in-root dig) reads as Dig — the
        // pre-tier behavior, byte-identical.
        let legacy: HitFrame = serde_json::from_str(
            r#"{"frame":"batch","seq":1,"hits":[{"path":"/w/src/a.rs","line":3,"column":1,"text":"fn f() {"}]}"#,
        )
        .expect("parse legacy batch");
        assert!(
            matches!(
                legacy,
                HitFrame::Batch {
                    tier: WalkTier::Dig,
                    ..
                }
            ),
            "absent tier reads as Dig (project-grade, today's behavior)"
        );
    }

    #[test]
    fn hit_frame_carries_no_observation_channel() {
        // Bug 146's final contraction on the wire: neither the batch nor the
        // terminator carries observations or reap scopes. The search walk is
        // pure search; the daemon observes for itself, targeted.
        let batch = serde_json::to_string(&HitFrame::batch(1, vec![sample_hit()]))
            .expect("serialize batch");
        assert!(
            !batch.contains("observed"),
            "no observation channel on a batch: {batch}"
        );
        let end = serde_json::to_string(&HitFrame::end(3)).expect("serialize end");
        assert!(line_is_clean(&end), "no observations, no reap claim: {end}");
        assert!(end.contains("\"frame\":\"end\""), "end carries the tag");
        let back: HitFrame = serde_json::from_str(&end).expect("parse end");
        assert_eq!(back, HitFrame::end(3), "hit end roundtrips");
    }

    /// Whether a serialized terminator is free of the retired observation
    /// fields.
    fn line_is_clean(line: &str) -> bool {
        !line.contains("observed") && !line.contains("reap_scopes")
    }

    #[test]
    fn legacy_observation_fields_are_ignored() {
        // An older CLI still ships `observed`/`reap_scopes`. They are unknown
        // fields now: parsed away, never acted on — skew degrades to the
        // daemon observing for itself, which is the whole behavior.
        let legacy: HitFrame = serde_json::from_str(
            r#"{"frame":"end","batches":2,"observed":[["/w/src/a.rs",42]],"reap_scopes":["/w"]}"#,
        )
        .expect("parse legacy end");
        assert_eq!(legacy, HitFrame::end(2));

        let legacy: HitFrame = serde_json::from_str(
            r#"{"frame":"batch","seq":1,"hits":[{"path":"/w/src/a.rs","line":3,"column":1,"text":"fn f() {"}],"observed":[["/w/src/a.rs",7]]}"#,
        )
        .expect("parse legacy batch");
        assert_eq!(legacy, HitFrame::batch(1, vec![sample_hit()]));
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
        let annotated = AnnotatedHit::scoped(sample_hit(), "mod_a/f".to_string());
        assert_eq!(annotated.render(), "/w/src/a.rs:3:1#mod_a/f:fn f() {");
    }

    // ─── grep line shape (ws43-02: the cutover rendering) ──────────────────

    #[test]
    fn grep_line_scoped_hit_carries_the_trail() {
        let annotated = AnnotatedHit::scoped(sample_hit(), "mod_a/f".to_string());
        assert_eq!(
            annotated.render_grep_line("src/a.rs"),
            "src/a.rs:3#mod_a/f:fn f() {"
        );
    }

    #[test]
    fn grep_line_top_level_hit_has_no_anchor() {
        let annotated = AnnotatedHit::top_level(sample_hit());
        assert_eq!(
            annotated.render_grep_line("src/a.rs"),
            "src/a.rs:3:fn f() {"
        );
    }

    #[test]
    fn grep_line_unenriched_hit_marks_could_not_enrich() {
        // A pass-through (or budget-blown, or no-server) hit renders the `#?`
        // marker — degradation is never misread as top-level.
        let annotated = AnnotatedHit::passthrough(sample_hit());
        assert_eq!(
            annotated.render_grep_line("src/a.rs"),
            "src/a.rs:3#?:fn f() {"
        );
    }

    #[test]
    fn grep_line_passthrough_matches_unannotated_grep_spelling() {
        // The degrade-only invariant in the grep shape: a pass-through
        // annotation-batch and a daemon-absent batch print the same bytes.
        let hit = sample_hit();
        let annotated = AnnotatedHit::passthrough(hit.clone());
        assert_eq!(
            annotated.render_grep_line("src/a.rs"),
            hit.render_grep_unannotated("src/a.rs")
        );
    }

    #[test]
    fn enriched_flag_roundtrips_and_defaults_false() {
        let annotated = AnnotatedHit::top_level(sample_hit());
        let line = serde_json::to_string(&annotated).expect("serialize");
        assert!(
            line.contains("\"enriched\":true"),
            "flag on the wire: {line}"
        );
        let back: AnnotatedHit = serde_json::from_str(&line).expect("parse");
        assert_eq!(back, annotated);

        // A frame from a peer that predates the field parses as could-not-enrich
        // (the honest degrade reading), never as top-level.
        let legacy = r#"{"path":"/w/src/a.rs","line":3,"column":1,"text":"fn f() {"}"#;
        let parsed: AnnotatedHit = serde_json::from_str(legacy).expect("parse legacy");
        assert!(!parsed.enriched, "absent field reads as unenriched");
        assert_eq!(parsed.render_grep_line("src/a.rs"), "src/a.rs:3#?:fn f() {");
    }

    // ─── glob outline carriage (ws43-03) ────────────────────────────────────

    #[test]
    fn outline_fields_roundtrip_and_default_absent() {
        let annotated = AnnotatedHit {
            hit: sample_hit(),
            anchor: None,
            enriched: true,
            outline: Some("3  fn f() {\n\t5  fn g() {".to_string()),
            suppressed: false,
        };
        let line = serde_json::to_string(&annotated).expect("serialize");
        assert!(line.contains("\"outline\""), "outline on the wire: {line}");
        let back: AnnotatedHit = serde_json::from_str(&line).expect("parse");
        assert_eq!(back, annotated, "outline roundtrips");

        // An anchor-shaped hit from an old daemon (no outline fields) parses
        // outline-less and unsuppressed — the CLI renders the listing
        // unenriched, never fewer paths.
        let legacy =
            r#"{"path":"/w/src/a.rs","line":3,"column":1,"text":"fn f() {","enriched":true}"#;
        let parsed: AnnotatedHit = serde_json::from_str(legacy).expect("parse legacy");
        assert!(parsed.outline.is_none(), "absent outline reads as None");
        assert!(!parsed.suppressed, "absent suppressed reads as false");

        // A grep hit never serializes the glob fields — the grep wire is
        // byte-identical to the pre-weight protocol.
        let grep_hit = serde_json::to_string(&AnnotatedHit::top_level(sample_hit()))
            .expect("serialize grep hit");
        assert!(
            !grep_hit.contains("outline") && !grep_hit.contains("suppressed"),
            "grep hits carry no glob fields: {grep_hit}"
        );
    }

    #[test]
    fn suppressed_flag_roundtrips() {
        let annotated = AnnotatedHit {
            hit: sample_hit(),
            anchor: None,
            enriched: true,
            outline: None,
            suppressed: true,
        };
        let line = serde_json::to_string(&annotated).expect("serialize");
        assert!(
            line.contains("\"suppressed\":true"),
            "suppression on the wire: {line}"
        );
        let back: AnnotatedHit = serde_json::from_str(&line).expect("parse");
        assert_eq!(back, annotated);
    }
}
