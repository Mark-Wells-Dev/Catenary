// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! The hit-batch frame protocol and the CLI-owns-the-walk skeleton (ws43).
//!
//! The ruled rework: the CLI owns the walk and the match, always — one engine.
//! The daemon stops being a query executor and becomes a **bounded enrichment
//! annotator** on a streamed hit protocol. Every dependency failure degrades to
//! "less enrichment," never "no results."
//!
//! Since the ws43-02/-03 cutovers this IS `catenary grep` **and** `catenary
//! glob`: the CLI walks (grep through [`engine`], glob through the plan build
//! in `bridge::file_tools`), streams through the sinks below, and the daemon's
//! only search surface is the `tool/hitstream` annotation arm (the `tool/grep`
//! and `tool/glob` executor arms retired). Glob batches carry a CLI-computed
//! [`EnrichmentWeight`] (the ruled listing-weight lever: listing shapes get
//! top-level structure by default, `--outline` opts up); grep batches carry
//! none. The load-bearing structure:
//!
//! 1. The wire protocol — [`HitFrame`] (CLI → daemon) and [`AnnotationFrame`]
//!    (daemon → CLI), an internally-tagged frame stream on the existing socket,
//!    with an [`HitFrame::End`] / [`AnnotationFrame::End`] terminator and honest
//!    unknown-frame rejection on both sides. Since ws43-02 an annotated hit
//!    carries the executor's tri-state anchor (`#trail` / top-level / `#?`),
//!    and [`frame::AnnotatedHit::render_grep_line`] reproduces today's grep
//!    output shape from it.
//! 2. The CLI walk ([`walk`]) that walks and matches — the full `catenary grep`
//!    flag surface since ws43-02, through the query executor's own
//!    matcher/searcher constructors — emitting **ordered** [`HitBatch`]es of
//!    canonical-path hits, and two sinks selectable at the seam:
//!    [`stdout_unannotated`] (the degrade path — built first) and
//!    [`daemon_stream`] (batches out, annotation-batches back, ordered emission
//!    preserved).
//! 3. The daemon annotator ([`annotate_connection`]) — read batch → await
//!    (budgeted) → write batch, a native async citizen. Since ws43-02 the
//!    router serves the REAL enricher ([`crate::bridge::HitstreamEnricher`]:
//!    the executors' LSP enrichment, migrated), with the WS31 observation
//!    nudge and the query auto-mount (ws43-05 sensitive-path gate included)
//!    riding each annotation call.
//!
//! ## Invariants (ruled; not renegotiable)
//!
//! - **Complete output.** Budgets apply to *enrichment* only, never to hits.
//!   Streaming preserves the contract; truncation does not exist.
//! - **Degrade-only.** No daemon, wedged daemon, blown budget, old daemon → the
//!   identical result stream, less annotation, never fewer results. An old daemon
//!   answering unknown-method lands on the SAME fallback as daemon-absent.
//! - **Stream discipline, by construction.** Results on stdout, advisories on
//!   stderr, every line write atomic. The [`ResultSink`] is the only writer of a
//!   result line, and it frames each line as one buffered `write_all` — the class
//!   of bug where a stderr hint fuses into a stdout result line under piping is
//!   impossible here because a result line is one atomic write and advisories
//!   never travel the result channel.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

pub mod annotator;
pub mod engine;
pub mod frame;
pub mod sink;

pub use annotator::{BatchEnricher, PassThroughEnricher, annotate_connection, serve_passthrough};
pub use engine::{Hit, HitBatch, WalkOptions, WalkSummary, walk};
pub use frame::{
    AnnotatedBatch, AnnotatedHit, AnnotationFrame, AnnotationVerdict, EnrichmentWeight, HitFrame,
};
pub use sink::{
    DaemonStreamReport, GrepRender, ResultSink, annotate_paths, daemon_stream, stdout_unannotated,
};

/// The IPC method string the CLI sends as the hit-stream handshake's first line.
///
/// Owned here (not in `router`) so the protocol string lives with the protocol;
/// the router re-exports it as `METHOD_HITSTREAM`. An old daemon that predates
/// this method never matches the dispatch arm and falls through to the
/// unknown-method tail, degrading the CLI to the unannotated stream.
pub const HITSTREAM_METHOD: &str = "tool/hitstream";

/// The per-batch enrichment budget: a timeout on the awaited annotation future.
///
/// Generalizes the query-path [`crate::lsp::manager::QUERY_ENRICHMENT_BUDGET`]
/// pattern. A budget bounds *enrichment*, never hits — a blown budget yields a
/// pass-through verdict on a complete, unannotated batch, never a dropped hit.
pub const ANNOTATION_BATCH_BUDGET: Duration = Duration::from_secs(5);

/// Ordered batches of at most this many hits. Small by design so the daemon's
/// annotation await stays granular and the in-flight window pipelines without
/// buffering a whole result set anywhere.
pub const HIT_BATCH_SIZE: usize = 64;

/// The number of hit-batches the daemon-stream sink keeps in flight at once.
///
/// The window bounds latency-hiding, never ordering: [`sink::daemon_stream`]
/// reassembles annotation-batches into batch-sequence order before emission, so a
/// slow batch delays but never reorders (the ordered-emission invariant).
pub const IN_FLIGHT_WINDOW: usize = 4;

/// The CLI-side per-read deadline on the annotation stream (ws43-02).
///
/// An accepts-then-silent daemon — the connection opened, batches were taken,
/// no annotation ever comes back — is otherwise indistinguishable from a slow
/// one. The deadline is a generous wall-clock bound on the gap between
/// annotation frames, comfortably above [`ANNOTATION_BATCH_BUDGET`] (the
/// daemon's own per-batch ceiling, which it answers within even when
/// enrichment blows), and it is armed **only while an annotation is
/// outstanding** — a long quiet stretch of the walk with nothing owed never
/// trips it. Expiry is degrade-only: the stream completes unannotated in
/// place, never fewer results.
pub const STREAM_READ_DEADLINE: Duration = Duration::from_secs(30);

/// Canonicalizes a path at the walk ingestion seam (the hit-batch carries
/// canonical paths).
///
/// The walk is a path-spelling seam: a hit path keys a ledger read (the daemon's
/// `resolve_root` canonical-prefix check) and compares against stored canonical
/// roots downstream, so it must be canonicalized here, once, mirroring the grep
/// server's cwd canonicalization and the glob server's pattern-base
/// canonicalization (misc 193). A path that does not resolve (a live-race
/// deletion) keeps its spelling — `canonicalize` cannot resolve it — so a hit is
/// never dropped for a spelling that momentarily fails to resolve.
#[must_use]
pub fn canonicalize_hit_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// A single grep hit, canonical-path, as it crosses the wire.
///
/// The atom the walk emits and the daemon annotates. `path` is canonical (see
/// [`canonicalize_hit_path`]); `line` is 1-based (ripgrep display convention);
/// `column` is the 1-based column of the first match on the line, or `0` when
/// the line carries no match — a context line (`-A`/`-B`/`-C`) or an inverted
/// selection (`-v`), which are hits with no match column (mirroring the query
/// engine's convention); `text` is the full matched source line, verbatim,
/// newline stripped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireHit {
    /// Canonical absolute path of the matched file.
    pub path: PathBuf,
    /// 1-based line of the match.
    pub line: u32,
    /// 1-based column of the first match on the line; `0` for a line with no
    /// match (context or inverted selection).
    pub column: u32,
    /// The full source line at the hit, verbatim and newline-stripped.
    pub text: String,
}

impl WireHit {
    /// Renders this hit as one self-contained result line, unannotated:
    /// `path:line:column:text`. The protocol skeleton's wire-debug spelling — no
    /// `#scope`, no anchor — so a hit that never reaches the annotator still
    /// prints a complete, grep-parseable line. The user-visible `catenary grep`
    /// degrade spelling is [`Self::render_grep_unannotated`].
    #[must_use]
    pub fn render_unannotated(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.path.display(),
            self.line,
            self.column,
            self.text
        )
    }

    /// Renders this hit as one `catenary grep` result line in the degrade
    /// spelling: `display:line#?:text` — the could-not-enrich marker, exactly
    /// what today's daemon-less twin prints for a hit with no covering server.
    /// Byte-identical to a pass-through
    /// [`AnnotatedHit`](frame::AnnotatedHit)'s
    /// [`render_grep_line`](frame::AnnotatedHit::render_grep_line), which is
    /// what makes daemon-absent, wedged-daemon, old-daemon, and blown-budget
    /// streams print the same result bytes (ws43-02).
    #[must_use]
    pub fn render_grep_unannotated(&self, display_path: &str) -> String {
        format!("{display_path}:{}#?:{}", self.line, self.text)
    }
}

/// Reads one newline-delimited JSON frame of type `F` from `reader`.
///
/// Returns `Ok(None)` at a clean EOF (the peer closed the stream after its
/// terminator), `Ok(Some(frame))` for a parsed frame, and an error for a partial
/// line or an unrecognized frame kind — honest degradation, never a silent
/// misparse. An unknown frame tag deserializes to a comprehensible error (the
/// internally-tagged enum rejects an unrecognized `"frame"` value), which the
/// caller treats as a version-skew fallback signal.
///
/// # Errors
///
/// Returns an error if the underlying read fails or the line is not a valid
/// frame of type `F`.
#[allow(
    clippy::future_not_send,
    reason = "generic over the reader; callers that spawn (daemon_stream) supply Send types, callers that stay on one task (annotate_connection) need no Send"
)]
pub async fn read_frame<R, F>(reader: &mut R, line: &mut String) -> Result<Option<F>>
where
    R: tokio::io::AsyncBufRead + Unpin,
    F: for<'de> Deserialize<'de>,
{
    use tokio::io::AsyncBufReadExt;

    line.clear();
    let n = reader
        .read_line(line)
        .await
        .context("read hitstream frame")?;
    if n == 0 {
        return Ok(None);
    }
    let frame: F = serde_json::from_str(line.trim_end())
        .with_context(|| format!("parse hitstream frame: {}", line.trim_end()))?;
    Ok(Some(frame))
}

/// Writes one frame as a single JSON line, atomically.
///
/// The frame is serialized into one owned buffer with its trailing newline, then
/// flushed with a single `write_all` — one line, one write, never interleaved
/// with another writer's bytes. This is the wire-side leg of the
/// stream-discipline invariant.
///
/// # Errors
///
/// Returns an error if serialization or the write fails.
#[allow(
    clippy::future_not_send,
    reason = "generic over the writer; callers that spawn (daemon_stream) supply Send types, callers that stay on one task (annotate_connection) need no Send"
)]
pub async fn write_frame<W, F>(writer: &mut W, frame: &F) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    F: Serialize,
{
    use tokio::io::AsyncWriteExt;

    let mut bytes = serde_json::to_vec(frame).context("serialize hitstream frame")?;
    bytes.push(b'\n');
    writer
        .write_all(&bytes)
        .await
        .context("write hitstream frame")?;
    Ok(())
}

/// Flushes one advisory line to `stderr`, atomically and independently of the
/// result channel.
///
/// Advisories NEVER travel the result channel: they are a distinct stream by
/// construction. A single `write_all` of the whole line (its newline included)
/// means a hint can never fuse mid-line into a stdout result under piping — the
/// live bug this construction makes impossible.
///
/// # Errors
///
/// Returns an error if the write fails.
pub fn advise(message: &str) -> Result<()> {
    let mut line = String::with_capacity(message.len() + 1);
    line.push_str(message);
    line.push('\n');
    let stderr = std::io::stderr();
    let mut handle = stderr.lock();
    handle
        .write_all(line.as_bytes())
        .map_err(|e| anyhow!("write advisory: {e}"))?;
    handle.flush().map_err(|e| anyhow!("flush advisory: {e}"))?;
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn wire_hit_renders_unannotated_line() {
        let hit = WireHit {
            path: PathBuf::from("/w/src/a.rs"),
            line: 12,
            column: 5,
            text: "fn main() {".to_string(),
        };
        assert_eq!(hit.render_unannotated(), "/w/src/a.rs:12:5:fn main() {");
    }

    #[test]
    fn canonicalize_unresolvable_path_keeps_spelling() {
        // A path that does not exist keeps its spelling rather than vanishing —
        // a hit is never dropped for a momentarily-unresolvable spelling.
        let ghost = PathBuf::from("/nonexistent/ws43/ghost.rs");
        assert_eq!(canonicalize_hit_path(&ghost), ghost);
    }

    #[tokio::test]
    async fn read_frame_reports_eof_as_none() {
        let bytes: &[u8] = b"";
        let mut reader = tokio::io::BufReader::new(bytes);
        let mut line = String::new();
        let got: Option<HitFrame> = read_frame(&mut reader, &mut line)
            .await
            .expect("read at eof");
        assert!(got.is_none(), "clean eof is None, not an error");
    }
}
