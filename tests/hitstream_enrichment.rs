// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

#![deny(clippy::unwrap_used, clippy::panic)]
#![allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
//! Daemon-level pins for the ws43-02 enrichment migration: the `tool/hitstream`
//! arm serves the REAL grep enrichment.
//!
//! These tests speak the raw hit-batch frame protocol to a live daemon over its
//! IPC socket — exactly what the cut-over `catenary grep` CLI does — and pin:
//!
//! - **Enriched parity**: an annotation-batch for a covered-root hit carries the
//!   `#scope` anchor, and `render_grep_line` reproduces the full CLI's grep
//!   output line byte-for-byte (the pin that once compared against the retired
//!   `tool/grep` executor now closes the loop against the CLI itself).
//! - **The WS31 observation nudge fires from the annotation call**: a hit-batch
//!   alone (no grep CLI run) routes `didChangeWatchedFiles` for the batch's
//!   files.
//! - **Query auto-mount fires from the annotation call**: a batch whose hits lie
//!   outside every tracked root mounts the enclosing project root, and the hits
//!   enrich from the freshly-attached server.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use catenary_cli::hitstream::frame::{AnnotatedBatch, AnnotationFrame, HitFrame};
use catenary_cli::hitstream::{HITSTREAM_METHOD, WireHit};
use common::{
    BridgeProcess, grep_until_enriched, mockls_lsp_arg, read_merged_log, watched_file_changes,
};

const MOCK_LANG: &str = "mockls-event";

/// Poll ceiling for the retry loops below (server settle, mount spawn).
const PIN_BACKSTOP: Duration = Duration::from_secs(30);

/// Opens one `tool/hitstream` connection, streams `hits` as a single batch plus
/// the terminator, and returns the annotation-batches the daemon answered.
fn hitstream_exchange(socket: &Path, hits: &[WireHit]) -> Result<Vec<AnnotatedBatch>> {
    let mut stream =
        std::os::unix::net::UnixStream::connect(socket).context("connect hitstream socket")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .context("set read timeout")?;

    // The method-line handshake, then the frames — one JSON object per line.
    let mut payload = serde_json::to_vec(&json!({ "method": HITSTREAM_METHOD }))?;
    payload.push(b'\n');
    let batch = HitFrame::batch(0, hits.to_vec());
    payload.extend(serde_json::to_vec(&batch)?);
    payload.push(b'\n');
    payload.extend(serde_json::to_vec(&HitFrame::end(1))?);
    payload.push(b'\n');
    stream.write_all(&payload).context("write hit frames")?;
    stream.flush().context("flush hit frames")?;

    let reader = BufReader::new(stream);
    let mut batches = Vec::new();
    for line in reader.lines() {
        let line = line.context("read annotation frame")?;
        let frame: AnnotationFrame =
            serde_json::from_str(&line).with_context(|| format!("parse frame: {line}"))?;
        match frame {
            AnnotationFrame::Batch { batch } => batches.push(batch),
            AnnotationFrame::End { .. } => return Ok(batches),
        }
    }
    bail!("annotation stream ended without a terminator")
}

/// Enriched parity: the annotation-batch for a covered-root hit carries the
/// containment anchor, and the grep-shape rendering matches the full
/// `catenary grep` CLI output byte-for-byte (the ws43-02 acceptance byte-parity
/// pin, end to end).
#[test]
fn hitstream_annotation_matches_executor_enrichment() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let file = dir.path().join(format!("code.{MOCK_LANG}"));
    std::fs::write(&file, "struct Outer {\nfn inner\n}\nfn leaf\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;

    // Warm the CLI path first: it retries until the `#Outer` anchor
    // appears, proving the server settled and the symbol index is populated.
    // Its output is the parity target.
    let executor_out = grep_until_enriched(&bridge, &json!({ "pattern": "inner" }))?;
    let executor_line = executor_out
        .lines()
        .find(|l| l.contains("fn inner"))
        .context("executor grep line for `fn inner`")?
        .to_string();
    assert!(
        executor_line.contains("#Outer:"),
        "executor line carries the scope anchor: {executor_line}"
    );

    // The same hit, as the CLI-side walk would emit it: canonical path,
    // 1-based line, verbatim text.
    let hit = WireHit {
        path: file.clone(),
        line: 2,
        column: 4,
        text: "fn inner".to_string(),
    };
    let socket = bridge.wait_for_ipc_socket()?;
    let batches = hitstream_exchange(&socket, std::slice::from_ref(&hit))?;
    assert_eq!(batches.len(), 1, "one annotation-batch per hit-batch");
    let batch = &batches[0];
    assert_eq!(batch.seq, 0);
    assert_eq!(batch.hits.len(), 1, "every hit survives annotation");

    let annotated = &batch.hits[0];
    assert!(annotated.enriched, "a covered-root hit is enriched");
    assert_eq!(
        annotated.anchor.as_deref(),
        Some("Outer"),
        "the annotator derives the executor's containment trail"
    );

    // Byte parity: rendering the annotated hit with the executor's display path
    // reproduces the executor's output line exactly.
    let rel = file
        .strip_prefix(dir.path())
        .context("relativize hit path")?
        .to_string_lossy()
        .into_owned();
    assert_eq!(
        annotated.render_grep_line(&rel),
        executor_line,
        "annotation-batch rendering is byte-identical to the executor's line"
    );
    Ok(())
}

/// The WS31 observation nudge fires from the annotation call: a hit-batch alone
/// (no `tool/grep` query) routes `didChangeWatchedFiles` for the batch's file.
#[test]
fn hitstream_annotation_call_routes_the_observation_nudge() -> Result<()> {
    let dir = common::canonical_tempdir()?;
    let log_path = dir.path().join("notifications.jsonl");
    let file = dir.path().join(format!("a.{MOCK_LANG}"));
    std::fs::write(&file, "needle\n")?;

    let log_arg = log_path.to_str().context("log path")?;
    let lsp = mockls_lsp_arg(
        MOCK_LANG,
        &format!(
            "--register-file-watchers --watcher-glob **/*.{MOCK_LANG} \
             --notification-log {log_arg}"
        ),
    );
    let root = dir.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    let hit = WireHit {
        path: file.clone(),
        line: 1,
        column: 1,
        text: "needle".to_string(),
    };
    let a_uri = format!("file://{}", file.display());

    // The annotator nudges after a bounded settle wait; if the watcher
    // registration raced the first batch, a later batch re-nudges (the baseline
    // stays cold until a nudge actually runs), so retry the exchange until the
    // cold-baseline `Changed(2)` lands in the mock server's log.
    let deadline = Instant::now() + PIN_BACKSTOP;
    loop {
        let _ = hitstream_exchange(&socket, std::slice::from_ref(&hit))?;
        let log = read_merged_log(&log_path);
        let announced = watched_file_changes(&log)
            .iter()
            .any(|(u, t)| *u == a_uri && *t == 2);
        if announced {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("annotation call never routed didChangeWatchedFiles for {a_uri}; log:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Query auto-mount fires from the annotation call: a batch whose hit lies
/// outside every tracked root mounts the enclosing project root (repository
/// marker walk), and the hit enriches from the freshly-attached server.
#[test]
fn hitstream_out_of_root_batch_mounts_and_enriches() -> Result<()> {
    // The daemon's tracked root: an unrelated directory.
    let tracked = common::canonical_tempdir()?;
    // The out-of-root project: a repository-marked tree the daemon does not track.
    let project = common::canonical_tempdir()?;
    std::fs::create_dir_all(project.path().join(".git"))?;
    let file = project.path().join(format!("code.{MOCK_LANG}"));
    std::fs::write(&file, "struct Outer {\nfn inner\n}\n")?;

    let lsp = mockls_lsp_arg(MOCK_LANG, "");
    let root = tracked.path().to_str().context("root path")?;
    let mut bridge = BridgeProcess::spawn(&[&lsp], root)?;
    bridge.initialize()?;
    let socket = bridge.wait_for_ipc_socket()?;

    let hit = WireHit {
        path: file,
        line: 2,
        column: 4,
        text: "fn inner".to_string(),
    };

    // The first exchange triggers the ephemeral mount (server spawn + settle may
    // outlive that batch's budget — a pass-through verdict, degrade-only);
    // retries find the mount warm and the hit enriched. Without the mount this
    // could never succeed: no tracked root covers the project, so no server
    // would ever serve it.
    let deadline = Instant::now() + PIN_BACKSTOP;
    loop {
        let batches = hitstream_exchange(&socket, std::slice::from_ref(&hit))?;
        assert_eq!(batches.len(), 1, "one annotation-batch per hit-batch");
        assert_eq!(
            batches[0].hits.len(),
            1,
            "the hit survives whichever verdict the batch got"
        );
        let annotated = &batches[0].hits[0];
        if annotated.enriched && annotated.anchor.as_deref() == Some("Outer") {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!(
                "out-of-root hit never enriched (anchor={:?}, enriched={}) — \
                 the annotation-call auto-mount did not take",
                annotated.anchor,
                annotated.enriched
            );
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}
