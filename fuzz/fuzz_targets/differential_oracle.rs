// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Coverage-guided differential fuzz target for the faithful shell parser
//! (ADR 020 §6, tokenizer ticket 06).
//!
//! The body is the *same* [`oracle::check`] the `proptest` layer drives — reused
//! verbatim through the parent crate's `fuzzing` feature, no duplicated oracle
//! logic. `check` parses the input with our hand-rolled parser and with the
//! `brush-parser` reference, projects both to the gate's view (command-position
//! words + redirect-operator signature), and asserts they agree; a disagreement
//! `panic!`s, which libFuzzer records as a crash artifact to minimize.
//!
//! Per ADR 020 §6 the property is *correctness*, not robustness: the soak hunts
//! for over-counting (a false-deny — the agent's everyday pain) and under-counting
//! (a missed command that wedges the daemon / hides an edit), over the long tail
//! `proptest`'s structured generators won't reach. `fuzz/corpus/` is seeded with
//! the known bug 11/13/17/20/30/33 repros so they stay permanent regressions.
//!
//! Run out-of-band on nightly (`make fuzz` / see `../README.md`); never CI-stable.

#![no_main]

use libfuzzer_sys::fuzz_target;

use catenary_cli::cli::command_filter::oracle;

fuzz_target!(|data: &[u8]| {
    // The gate receives its input as a JSON string field — always valid UTF-8 —
    // so normalize raw libFuzzer bytes through a lossy decode rather than
    // discarding non-UTF-8 inputs. This still exercises the parser's byte-level
    // `from_utf8_lossy` recovery path while matching the production input shape.
    let input = String::from_utf8_lossy(data);
    oracle::check(&input);
});
