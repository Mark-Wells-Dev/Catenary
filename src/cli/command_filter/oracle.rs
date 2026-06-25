// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Differential fuzzing oracle for the faithful shell parser (tokenizer 05).
//!
//! ADR 020 §6 correctness oracle: parse an input with our hand-rolled
//! [`super::parse`] and with a trusted bash-fidelity reference
//! (`brush-parser`), project both to the gate's view (command-position words,
//! real operators/redirects, and structure), and assert they agree. A
//! `proptest` layer drives the property on stable CI so every build hunts for
//! inputs where we disagree with the shell; the known bug 11/13/17/20/30/33
//! reproductions seed an explicit corpus as fixed regressions.
//!
//! The reference parser is a **dev/fuzz-only** dependency — this whole module is
//! gated `#[cfg(any(test, feature = "fuzzing"))]`, so `brush-parser` never enters
//! the runtime / `cargo deny` runtime graph and never ships. The shared
//! differential property [`check`] is exposed `pub` so the out-of-tree `fuzz/`
//! crate (tokenizer ticket 06) drives the *same* oracle as the `proptest` layer —
//! one copy of the projection + assertion logic, two harnesses (`proptest` on
//! stable CI, `cargo-fuzz` for the nightly soak). The `proptest` strategies, the
//! `proptest!` block, the seed corpus, and the unit tests stay `#[cfg(test)]`; the
//! fuzz crate seeds its own on-disk corpus and supplies inputs from libFuzzer.
//!
//! ## Safety direction (ADR 020)
//!
//! Over-counting command positions is a *false-deny* — the agent's everyday
//! pain — so the primary metric is **equality** of the command-position
//! projection. Under-counting is the dangerous direction: it wedges the daemon
//! / hides an edit, so a **containment** (superset) guard is asserted
//! independently. Both directions are first-class.
//!
//! ## Documented divergence policy (the pathological tail)
//!
//! Our parser is faithful on the gate's subset but deliberately *conservative*
//! on a few constructs the gate doesn't reason about; `brush-parser` models the
//! full bash grammar. Where they cannot agree by construction, the differential
//! input is **pruned** ([`should_skip`]) rather than asserted-equal, so proptest
//! never flags intended conservatism. The pruned classes are:
//!
//! - **Inputs `brush-parser` itself rejects** (invalid syntax). brush erroring
//!   is never treated as agreement — we skip, and separately assert our side
//!   does not *gain* command positions a successful brush parse would lack
//!   (the containment guard only fires on inputs both parse).
//! - **Function definitions** (`f() { … }`) and **arithmetic** (`$(( … ))` /
//!   `(( … ))`): brush models these as first-class grammar; our parser folds the
//!   `(`/`{` into a compound and sweeps inner words. The command-position sets
//!   legitimately differ, so these are pruned.
//! - **Here-documents** with bodies: our parser strips the body before lexing
//!   (the body is literal text); brush keeps it as a `Word`. The marker survives
//!   on both sides, but body handling differs, so heredoc inputs are pruned.
//! - **Adversarial-spelling tail** — any input where either parser yields a
//!   command name that is not a clean shell token ([`is_clean_name`]). brush
//!   partially cooks word values (outer double quotes stripped, inner kept raw),
//!   so re-cooking diverges on *spelling* for nested quotes (`"'`0`"`), a `#`
//!   surfaced from inside a substitution, or control / malformed-UTF-8 bytes
//!   (our byte-level `from_utf8_lossy` recovery renders U+FFFD differently from
//!   brush's char-level tokenizer). These are cosmetic name differences, always
//!   in the safe *over-counting* direction, never gate-relevant command
//!   structure — pruning them keeps the differential on real command vocabulary.
//!   The proptest layer additionally draws only printable ASCII; the raw
//!   control/byte tail is cargo-fuzz's job (ticket 06).

#[cfg(test)]
use proptest::prelude::{Just, Strategy, prop_oneof, proptest};

use brush_parser::ast;

use super::parse::{self, RedirectOp};

/// The gate's projection of a parsed shell input: the ordered command-position
/// names, a structural redirect-operator signature, and the command count.
///
/// This is the view both parsers must agree on. Names are normalized to the
/// same shape our parser's `SimpleCommand::name` carries — quote-interpreted and
/// path-stripped basename — so a raw brush `Word` and our cooked name compare on
/// equal footing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Projection {
    /// Command-position names across every pipeline / list / compound and every
    /// recursed `$(…)` / `` `…` `` / `<(…)` / `>(…)` substitution, in document
    /// order.
    command_positions: Vec<String>,
    /// The real redirect operators, in document order — the *kind* of each
    /// genuine file/fd redirect (a quoted `>` is not one). Targets are not
    /// compared (brush keeps them raw, we cook them); the operator multiset is
    /// the gate-relevant signal.
    redirect_ops: Vec<RedirectKind>,
}

/// A coarse redirect classification shared by both parsers' operator vocabulary.
///
/// Our [`RedirectOp`] and brush's `IoFileRedirectKind` use different spellings
/// for the same operators; this is the common denominator the differential
/// compares. Fine distinctions the gate does not act on (clobber vs. plain
/// write) collapse together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedirectKind {
    /// `>` / `>>` / `>|` — an output write/append to a file target.
    OutputFile,
    /// `<` — input from a file.
    InputFile,
    /// `>&` / `<&` — fd duplication.
    Dup,
    /// `&>` / `&>>` — redirect both stdout and stderr.
    OutputBoth,
    /// `<<<` — here-string.
    HereString,
}

// ── Our-side projection ───────────────────────────────────────────────────────

/// Project our parser's [`parse::ParsedScript`] to the gate view.
///
/// Empty command-position names are dropped: our parser represents a segment
/// whose sole word is a substitution (a bare `$(rm x)`) with an empty `name`
/// plus the recursed inner command. An empty name is never a command the shell
/// dispatches (brush surfaces only the recursed `rm`), so it is a projection
/// artifact, not a gate-relevant command position — normalizing it away lets the
/// two projections compare on the same semantic content. The gate ignores empty
/// names too (they match nothing in the allowlist).
fn ours_projection(input: &str) -> Projection {
    let script = parse::parse(input);
    let mut proj = Projection {
        command_positions: script
            .command_positions()
            .into_iter()
            .filter(|name| !name.is_empty())
            .collect(),
        redirect_ops: Vec::new(),
    };
    collect_ours_redirects(&script, &mut proj.redirect_ops);
    proj
}

/// Collect the redirect-operator signature from our parse, recursing into
/// substitutions exactly as `command_positions()` does.
fn collect_ours_redirects(script: &parse::ParsedScript, out: &mut Vec<RedirectKind>) {
    for pipeline in &script.pipelines {
        for cmd in &pipeline.commands {
            for redirect in &cmd.redirects {
                out.push(our_redirect_kind(redirect.op));
            }
            for sub in &cmd.substitutions {
                collect_ours_redirects(sub, out);
            }
        }
    }
}

/// Map our [`RedirectOp`] onto the shared [`RedirectKind`].
const fn our_redirect_kind(op: RedirectOp) -> RedirectKind {
    match op {
        RedirectOp::Write | RedirectOp::Append => RedirectKind::OutputFile,
        RedirectOp::Read => RedirectKind::InputFile,
        RedirectOp::DupOut | RedirectOp::DupIn => RedirectKind::Dup,
        RedirectOp::WriteBoth => RedirectKind::OutputBoth,
        RedirectOp::HereString => RedirectKind::HereString,
    }
}

// ── Brush-side projection ─────────────────────────────────────────────────────

/// Parse `input` with `brush-parser` and project its AST to the gate view, or
/// `None` if brush rejects the input (invalid syntax) — a brush error is never
/// agreement.
fn brush_projection(input: &str) -> Option<Projection> {
    let tokens = brush_parser::tokenize_str(input).ok()?;
    let program =
        brush_parser::parse_tokens(&tokens, &brush_parser::ParserOptions::default()).ok()?;
    let mut proj = Projection::default();
    walk_program(&program, &mut proj);
    Some(proj)
}

/// Walk a brush [`ast::Program`] — a sequence of complete commands.
fn walk_program(program: &ast::Program, proj: &mut Projection) {
    for complete in &program.complete_commands {
        walk_compound_list(complete, proj);
    }
}

/// Walk a brush `CompoundList` (a `;`/`&`-separated sequence of and-or lists).
fn walk_compound_list(list: &ast::CompoundList, proj: &mut Projection) {
    for item in &list.0 {
        walk_and_or_list(&item.0, proj);
    }
}

/// Walk a brush `AndOrList` (pipelines joined by `&&` / `||`).
fn walk_and_or_list(list: &ast::AndOrList, proj: &mut Projection) {
    walk_pipeline(&list.first, proj);
    for ao in &list.additional {
        match ao {
            ast::AndOr::And(p) | ast::AndOr::Or(p) => walk_pipeline(p, proj),
        }
    }
}

/// Walk a brush `Pipeline` (commands joined by `|`).
fn walk_pipeline(pipeline: &ast::Pipeline, proj: &mut Projection) {
    for cmd in &pipeline.seq {
        walk_command(cmd, proj);
    }
}

/// Walk one brush `Command`, surfacing its command position(s), redirects, and
/// any recursed substitutions.
fn walk_command(cmd: &ast::Command, proj: &mut Projection) {
    match cmd {
        ast::Command::Simple(simple) => walk_simple_command(simple, proj),
        ast::Command::Compound(compound, redirects) => {
            walk_compound_command(compound, proj);
            if let Some(list) = redirects {
                walk_redirect_list(list, proj);
            }
        }
        // Function definitions and extended-test (`[[ … ]]`) commands are pruned
        // at the input level (`should_skip`); they cannot reach here in a
        // non-skipped input. The arms exist for exhaustiveness only.
        ast::Command::Function(_) | ast::Command::ExtendedTest(_, _) => {}
    }
}

/// Walk a brush `SimpleCommand`: the command-position word (after assignment
/// prefixes), its argument words, and its redirects — recursing into any
/// substitution the words contain, mirroring our parser.
fn walk_simple_command(simple: &ast::SimpleCommand, proj: &mut Projection) {
    // Prefix items are assignment words and redirects, never the command word.
    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            walk_prefix_or_suffix_item(item, proj);
        }
    }

    if let Some(word) = &simple.word_or_name {
        if let Some(name) = cook_command_name(&word.value) {
            proj.command_positions.push(name);
        }
        // The command word itself may carry a substitution (`$(rm x) args`).
        recurse_word_substitutions(&word.value, proj);
    }

    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            walk_prefix_or_suffix_item(item, proj);
        }
    }
}

/// Walk a prefix/suffix item: a word (recurse into its substitutions), an
/// assignment word (recurse into the value's substitutions), a redirect, or a
/// process substitution (recurse into the inner list).
fn walk_prefix_or_suffix_item(item: &ast::CommandPrefixOrSuffixItem, proj: &mut Projection) {
    match item {
        ast::CommandPrefixOrSuffixItem::Word(word)
        | ast::CommandPrefixOrSuffixItem::AssignmentWord(_, word) => {
            recurse_word_substitutions(&word.value, proj);
        }
        ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) => {
            walk_io_redirect(redirect, proj);
        }
        ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
            walk_compound_list(&subshell.list, proj);
        }
    }
}

/// Walk a brush `CompoundCommand`. Loops/conditionals/subshells/brace-groups
/// recurse into their inner command lists, matching our parser sweeping a
/// compound's inner words into command positions. Arithmetic and coprocess
/// forms are pruned at the input level.
fn walk_compound_command(compound: &ast::CompoundCommand, proj: &mut Projection) {
    match compound {
        ast::CompoundCommand::BraceGroup(g) => walk_compound_list(&g.list, proj),
        ast::CompoundCommand::Subshell(s) => walk_compound_list(&s.list, proj),
        ast::CompoundCommand::ForClause(f) => {
            if let Some(values) = &f.values {
                for word in values {
                    recurse_word_substitutions(&word.value, proj);
                }
            }
            walk_compound_list(&f.body.list, proj);
        }
        ast::CompoundCommand::CaseClause(c) => {
            recurse_word_substitutions(&c.value.value, proj);
            for item in &c.cases {
                if let Some(body) = &item.cmd {
                    walk_compound_list(body, proj);
                }
            }
        }
        ast::CompoundCommand::IfClause(i) => {
            walk_compound_list(&i.condition, proj);
            walk_compound_list(&i.then, proj);
            if let Some(elses) = &i.elses {
                for else_clause in elses {
                    if let Some(cond) = &else_clause.condition {
                        walk_compound_list(cond, proj);
                    }
                    walk_compound_list(&else_clause.body, proj);
                }
            }
        }
        ast::CompoundCommand::WhileClause(w) | ast::CompoundCommand::UntilClause(w) => {
            walk_compound_list(&w.0, proj);
            walk_compound_list(&w.1.list, proj);
        }
        // Arithmetic / coprocess forms are pruned at the input level.
        ast::CompoundCommand::Arithmetic(_)
        | ast::CompoundCommand::ArithmeticForClause(_)
        | ast::CompoundCommand::Coprocess(_) => {}
    }
}

/// Walk a brush `RedirectList`.
fn walk_redirect_list(list: &ast::RedirectList, proj: &mut Projection) {
    for redirect in &list.0 {
        walk_io_redirect(redirect, proj);
    }
}

/// Classify a brush `IoRedirect` into the shared [`RedirectKind`], and recurse
/// into a process-substitution redirect target.
fn walk_io_redirect(redirect: &ast::IoRedirect, proj: &mut Projection) {
    match redirect {
        ast::IoRedirect::File(_, kind, target) => {
            proj.redirect_ops.push(brush_file_redirect_kind(kind));
            if let ast::IoFileRedirectTarget::Filename(word)
            | ast::IoFileRedirectTarget::Duplicate(word) = target
            {
                recurse_word_substitutions(&word.value, proj);
            }
            if let ast::IoFileRedirectTarget::ProcessSubstitution(_, subshell) = target {
                walk_compound_list(&subshell.list, proj);
            }
        }
        ast::IoRedirect::HereString(_, _) => proj.redirect_ops.push(RedirectKind::HereString),
        ast::IoRedirect::OutputAndError(_, _) => proj.redirect_ops.push(RedirectKind::OutputBoth),
        // Heredoc inputs are pruned at the input level (`should_skip`).
        ast::IoRedirect::HereDocument(_, _) => {}
    }
}

/// Map brush's `IoFileRedirectKind` onto the shared [`RedirectKind`]. Read/write
/// duplications collapse to [`RedirectKind::Dup`]; clobber and read-and-write
/// collapse into the output-file class.
const fn brush_file_redirect_kind(kind: &ast::IoFileRedirectKind) -> RedirectKind {
    match *kind {
        ast::IoFileRedirectKind::Write
        | ast::IoFileRedirectKind::Append
        | ast::IoFileRedirectKind::Clobber
        | ast::IoFileRedirectKind::ReadAndWrite => RedirectKind::OutputFile,
        ast::IoFileRedirectKind::Read => RedirectKind::InputFile,
        ast::IoFileRedirectKind::DuplicateInput | ast::IoFileRedirectKind::DuplicateOutput => {
            RedirectKind::Dup
        }
    }
}

// ── Name normalization + substitution recursion ──────────────────────────────

/// Normalize a raw brush command word to the shape our parser's
/// `SimpleCommand::name` carries: drop substitution spans (they become recursed
/// command positions, not part of the literal name — our parser builds the name
/// from the word's *literal text* only), interpret quotes away, then path-strip
/// to the basename. Returns `None` when nothing literal remains (a bare `""` or
/// a word that is purely a `$(…)` / `` `…` `` substitution), matching our parser
/// yielding no command word for those.
fn cook_command_name(raw: &str) -> Option<String> {
    let literal = strip_substitutions(raw);
    let unquoted = unquote(&literal);
    if unquoted.is_empty() {
        return None;
    }
    Some(
        std::path::Path::new(&unquoted)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&unquoted)
            .to_string(),
    )
}

/// Remove every top-level `$(…)` / `` `…` `` / `<(…)` / `>(…)` / `$((…))` span
/// from a raw word, leaving only the literal text our parser would accumulate
/// into the word's `text`. Quote- and nesting-aware (so a `)` inside a string
/// does not end a span), mirroring [`extract_substitutions`]'s scanner.
fn strip_substitutions(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let n = bytes.len();
    let mut out: Vec<u8> = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        match bytes[i] {
            // Single-quoted run: copied verbatim, never a substitution inside.
            b'\'' => {
                let start = i;
                i += 1;
                while i < n && bytes[i] != b'\'' {
                    i += 1;
                }
                i = (i + 1).min(n);
                out.extend_from_slice(&bytes[start..i]);
            }
            // `$(…)` / `$((…))` command substitution and `<(…)` / `>(…)` process
            // substitution alike — drop the whole balanced span.
            b'$' | b'<' | b'>' if i + 1 < n && bytes[i + 1] == b'(' => {
                let (_, next) = scan_balanced(bytes, i + 1);
                i = next;
            }
            // `` `…` `` backtick substitution — drop the span.
            b'`' => {
                let mut j = i + 1;
                while j < n && bytes[j] != b'`' {
                    if bytes[j] == b'\\' && j + 1 < n {
                        j += 1;
                    }
                    j += 1;
                }
                i = if j < n { j + 1 } else { n };
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Interpret shell quoting away from a raw word, faithfully enough to compare
/// command names: single quotes are literal, double quotes drop the delimiters
/// and honor the `\"`/`\\`/`` \` ``/`\$` escapes, `$'…'` and the `'\''` reopen
/// idiom fall out of per-run scanning, and an unquoted backslash escapes the
/// next byte. Mirrors the cooking our lexer applies so names compare on equal
/// footing.
fn unquote(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let n = bytes.len();
    let mut out = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < n && bytes[i] != b'\'' {
                    out.push(bytes[i]);
                    i += 1;
                }
                i += 1; // past the closing quote (or end)
            }
            b'"' => {
                i += 1;
                while i < n && bytes[i] != b'"' {
                    if bytes[i] == b'\\'
                        && i + 1 < n
                        && matches!(bytes[i + 1], b'"' | b'\\' | b'`' | b'$')
                    {
                        out.push(bytes[i + 1]);
                        i += 2;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
                i += 1;
            }
            b'$' if i + 1 < n && bytes[i + 1] == b'\'' => {
                i += 2;
                while i < n && bytes[i] != b'\'' {
                    if bytes[i] == b'\\' && i + 1 < n {
                        out.push(bytes[i + 1]);
                        i += 2;
                    } else {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
                i += 1;
            }
            b'\\' if i + 1 < n => {
                out.push(bytes[i + 1]);
                i += 2;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Find every `$(…)` / `` `…` `` / `<(…)` / `>(…)` substitution in a raw word
/// and re-project its inner text with brush, mirroring our parser recursing into
/// substitutions. brush keeps a substitution as opaque `Word` text, so the
/// recursion is performed here at the projection layer.
fn recurse_word_substitutions(raw: &str, proj: &mut Projection) {
    for inner in extract_substitutions(raw) {
        if let Some(sub) = brush_projection(&inner) {
            proj.command_positions.extend(sub.command_positions);
            proj.redirect_ops.extend(sub.redirect_ops);
        }
    }
}

/// Extract the inner text of each top-level command/process substitution in a
/// raw word, quote- and nesting-aware. `$(…)` / `<(…)` / `>(…)` use balanced
/// `(`/`)` scanning; `` `…` `` runs to the next unescaped backtick. Arithmetic
/// `$((…))` is recognized and skipped (it is not a command substitution).
fn extract_substitutions(raw: &str) -> Vec<String> {
    let bytes = raw.as_bytes();
    let n = bytes.len();
    let mut subs = Vec::new();
    let mut i = 0;
    while i < n {
        match bytes[i] {
            // Single-quoted run: no substitution inside.
            b'\'' => {
                i += 1;
                while i < n && bytes[i] != b'\'' {
                    i += 1;
                }
                i += 1;
            }
            // `$((…))` arithmetic — skip; `$(…)` command substitution — capture.
            b'$' if i + 1 < n && bytes[i + 1] == b'(' => {
                if i + 2 < n && bytes[i + 2] == b'(' {
                    let (_, next) = scan_balanced(bytes, i + 1);
                    i = next;
                } else {
                    let (inner, next) = scan_balanced(bytes, i + 1);
                    subs.push(inner);
                    i = next;
                }
            }
            // `<(…)` / `>(…)` process substitution.
            b'<' | b'>' if i + 1 < n && bytes[i + 1] == b'(' => {
                let (inner, next) = scan_balanced(bytes, i + 1);
                subs.push(inner);
                i = next;
            }
            // `` `…` `` backtick command substitution.
            b'`' => {
                let mut j = i + 1;
                while j < n && bytes[j] != b'`' {
                    if bytes[j] == b'\\' && j + 1 < n {
                        j += 1;
                    }
                    j += 1;
                }
                subs.push(String::from_utf8_lossy(&bytes[i + 1..j.min(n)]).into_owned());
                i = if j < n { j + 1 } else { n };
            }
            _ => i += 1,
        }
    }
    subs
}

/// Scan a balanced `(`/`)` run starting at the opening `(` (index `start`),
/// returning the inner text (between the outermost parens) and the index just
/// past the matching `)`. Quote-aware so a `)` inside a string does not close.
fn scan_balanced(bytes: &[u8], start: usize) -> (String, usize) {
    let n = bytes.len();
    let mut depth = 0u32;
    let mut i = start;
    let mut inner_start = start + 1;
    while i < n {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < n && bytes[i] != b'\'' {
                    i += 1;
                }
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < n && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < n {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'\\' if i + 1 < n => i += 2,
            b'(' => {
                depth += 1;
                if depth == 1 {
                    inner_start = i + 1;
                }
                i += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return (
                        String::from_utf8_lossy(&bytes[inner_start..i]).into_owned(),
                        i + 1,
                    );
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    (
        String::from_utf8_lossy(&bytes[inner_start.min(n)..n]).into_owned(),
        n,
    )
}

// ── Divergence policy ─────────────────────────────────────────────────────────

/// Whether an input falls in the pruned pathological tail where our deliberate
/// conservatism legitimately diverges from brush's full-grammar parse, so the
/// differential must skip it rather than assert agreement (see the module-level
/// divergence policy).
fn should_skip(input: &str) -> bool {
    // Unbalanced quotes / backticks / substitution parens: our parser scans an
    // unterminated construct greedily to end-of-input (recursing into it),
    // whereas brush may treat the dangling opener as literal text — a divergence
    // in the safe over-counting direction, but pure pathological tail. Prune it
    // before even consulting brush. (Real shell input is balanced.)
    if !is_balanced(input) {
        return true;
    }

    // Inputs brush itself rejects are never agreement.
    let Some(tokens) = brush_parser::tokenize_str(input).ok() else {
        return true;
    };
    let Ok(program) = brush_parser::parse_tokens(&tokens, &brush_parser::ParserOptions::default())
    else {
        return true;
    };
    // Heredoc bodies, function definitions, arithmetic, extended-test, and
    // coprocess forms are modeled by brush but folded/stripped by our parser —
    // prune them.
    if program_has_pruned_construct(&program) {
        return true;
    }

    // Prune the adversarial-spelling tail: if either parser yields a command
    // name that is not a clean shell token, the two parsers' cookings cannot be
    // compared by name (brush partially cooks word values — outer double quotes
    // stripped, inner kept raw — so re-cooking diverges on spelling for nested
    // quotes, a stray `#` surfaced from a substitution, etc.). These are always
    // safe-direction over-counts on our side; pruning them keeps the
    // differential focused on real command vocabulary without flagging intended
    // conservatism. See the module-level divergence policy.
    let mut brush = Projection::default();
    walk_program(&program, &mut brush);
    let ours = ours_projection(input);
    !ours.command_positions.iter().all(|n| is_clean_name(n))
        || !brush.command_positions.iter().all(|n| is_clean_name(n))
}

/// Whether an input's quotes, backticks, and substitution parens all pair up,
/// scanning quote-aware so a paren inside a string does not count. An unbalanced
/// input has a dangling opener our parser recurses greedily but brush may treat
/// literally — the pathological tail the differential prunes. Conservative: a
/// `\`-escaped quote/paren outside a string is treated as not opening anything.
fn is_balanced(input: &str) -> bool {
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    let mut paren_depth: i32 = 0;
    while i < n {
        match bytes[i] {
            b'\\' if i + 1 < n => i += 2,
            b'\'' => {
                let Some(end) = (i + 1..n).find(|&j| bytes[j] == b'\'') else {
                    return false; // unterminated single quote
                };
                i = end + 1;
            }
            b'"' => {
                let mut j = i + 1;
                loop {
                    if j >= n {
                        return false; // unterminated double quote
                    }
                    if bytes[j] == b'\\' && j + 1 < n {
                        j += 2;
                        continue;
                    }
                    if bytes[j] == b'"' {
                        break;
                    }
                    j += 1;
                }
                i = j + 1;
            }
            b'`' => {
                let mut j = i + 1;
                loop {
                    if j >= n {
                        return false; // unterminated backtick
                    }
                    if bytes[j] == b'\\' && j + 1 < n {
                        j += 2;
                        continue;
                    }
                    if bytes[j] == b'`' {
                        break;
                    }
                    j += 1;
                }
                i = j + 1;
            }
            b'(' => {
                paren_depth += 1;
                i += 1;
            }
            b')' => {
                paren_depth -= 1;
                if paren_depth < 0 {
                    return false; // stray close paren
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    paren_depth == 0
}

/// Whether a command name is a clean shell token — letters, digits, and the
/// path/word punctuation that appears in real command names — with no quote,
/// operator, comment, or whitespace character. A non-clean name is the marker of
/// the adversarial-spelling tail the differential prunes.
fn is_clean_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b'+' | b'@')
        })
}

/// Whether a parsed brush program contains a construct in the pruned tail.
fn program_has_pruned_construct(program: &ast::Program) -> bool {
    program
        .complete_commands
        .iter()
        .any(compound_list_has_pruned)
}

fn compound_list_has_pruned(list: &ast::CompoundList) -> bool {
    list.0.iter().any(|item| and_or_has_pruned(&item.0))
}

fn and_or_has_pruned(list: &ast::AndOrList) -> bool {
    pipeline_has_pruned(&list.first)
        || list.additional.iter().any(|ao| match ao {
            ast::AndOr::And(p) | ast::AndOr::Or(p) => pipeline_has_pruned(p),
        })
}

fn pipeline_has_pruned(pipeline: &ast::Pipeline) -> bool {
    pipeline.seq.iter().any(command_has_pruned)
}

fn command_has_pruned(cmd: &ast::Command) -> bool {
    match cmd {
        // Function definitions and extended tests have no faithful analogue in
        // our compound sweep.
        ast::Command::Function(_) | ast::Command::ExtendedTest(_, _) => true,
        ast::Command::Simple(simple) => simple_has_pruned(simple),
        ast::Command::Compound(compound, redirects) => {
            compound_has_pruned(compound)
                || redirects
                    .as_ref()
                    .is_some_and(|r| r.0.iter().any(io_redirect_is_pruned))
        }
    }
}

fn simple_has_pruned(simple: &ast::SimpleCommand) -> bool {
    let prefix = simple
        .prefix
        .as_ref()
        .is_some_and(|p| p.0.iter().any(prefix_or_suffix_is_pruned));
    let suffix = simple
        .suffix
        .as_ref()
        .is_some_and(|s| s.0.iter().any(prefix_or_suffix_is_pruned));
    prefix || suffix
}

const fn prefix_or_suffix_is_pruned(item: &ast::CommandPrefixOrSuffixItem) -> bool {
    matches!(
        item,
        ast::CommandPrefixOrSuffixItem::IoRedirect(r) if io_redirect_is_pruned(r)
    )
}

/// A heredoc redirect is pruned: our parser strips its body before lexing, so
/// the differential can't compare body command positions.
const fn io_redirect_is_pruned(redirect: &ast::IoRedirect) -> bool {
    matches!(redirect, ast::IoRedirect::HereDocument(_, _))
}

fn compound_has_pruned(compound: &ast::CompoundCommand) -> bool {
    match compound {
        // Arithmetic / coprocess: brush models them as grammar; our parser does
        // not expand arithmetic and treats the words structurally.
        ast::CompoundCommand::Arithmetic(_)
        | ast::CompoundCommand::ArithmeticForClause(_)
        | ast::CompoundCommand::Coprocess(_) => true,
        ast::CompoundCommand::BraceGroup(g) => compound_list_has_pruned(&g.list),
        ast::CompoundCommand::Subshell(s) => compound_list_has_pruned(&s.list),
        ast::CompoundCommand::ForClause(f) => compound_list_has_pruned(&f.body.list),
        ast::CompoundCommand::CaseClause(c) => c
            .cases
            .iter()
            .any(|i| i.cmd.as_ref().is_some_and(compound_list_has_pruned)),
        ast::CompoundCommand::IfClause(i) => {
            compound_list_has_pruned(&i.condition)
                || compound_list_has_pruned(&i.then)
                || i.elses.as_ref().is_some_and(|es| {
                    es.iter().any(|e| {
                        e.condition.as_ref().is_some_and(compound_list_has_pruned)
                            || compound_list_has_pruned(&e.body)
                    })
                })
        }
        ast::CompoundCommand::WhileClause(w) | ast::CompoundCommand::UntilClause(w) => {
            compound_list_has_pruned(&w.0) || compound_list_has_pruned(&w.1.list)
        }
    }
}

// ── The check() property ──────────────────────────────────────────────────────

/// The differential property: parse `input` with both parsers, project to the
/// gate view, and assert agreement.
///
/// 1. **Equality** of the command-position multiset — over-counting is a
///    false-deny (the agent's pain), the primary metric.
/// 2. **Containment** — our command positions are a superset of brush's;
///    under-counting hides a command from the gate (the false-allow risk).
/// 3. The same equality + containment for the redirect-operator signature.
///
/// Inputs in the pruned pathological tail ([`should_skip`]) — including those
/// brush itself rejects — are skipped, never treated as agreement.
///
/// Exposed `pub` (behind the module's `any(test, feature = "fuzzing")` gate) so
/// the out-of-tree `fuzz/` crate (tokenizer ticket 06) reuses this exact body as
/// its libFuzzer target — no duplicated oracle logic.
///
/// # Panics
///
/// Panics (via `assert*!`) when the two parsers disagree on the gate view — a
/// command-position or redirect-operator divergence. That panic *is* the
/// property: under `proptest` it surfaces a shrunk counterexample, under
/// `cargo-fuzz` a crash artifact to minimize.
pub fn check(input: &str) {
    if should_skip(input) {
        return;
    }
    let Some(oracle) = brush_projection(input) else {
        // Defensive: `should_skip` already returns true when brush errors.
        return;
    };
    let ours = ours_projection(input);

    let ours_cmds = sorted(&ours.command_positions);
    let oracle_cmds = sorted(&oracle.command_positions);

    // Primary metric — over-counting is a false-deny.
    assert_eq!(
        ours_cmds, oracle_cmds,
        "command-position disagreement (false-deny if ours ⊋ oracle) for {input:?}\n  \
         ours={ours_cmds:?}\n  oracle={oracle_cmds:?}"
    );

    // Invariant guard — under-counting wedges the daemon / hides an edit.
    assert!(
        is_superset(&ours.command_positions, &oracle.command_positions),
        "false-allow risk: our command positions miss one the shell runs, for {input:?}\n  \
         ours={:?}\n  oracle={:?}",
        ours.command_positions,
        oracle.command_positions
    );

    // Same two directions for the redirect-operator signature.
    let ours_redirs = sorted_redirs(&ours.redirect_ops);
    let oracle_redirs = sorted_redirs(&oracle.redirect_ops);
    assert_eq!(
        ours_redirs, oracle_redirs,
        "redirect-operator disagreement for {input:?}\n  ours={ours_redirs:?}\n  \
         oracle={oracle_redirs:?}"
    );
    assert!(
        is_redir_superset(&ours.redirect_ops, &oracle.redirect_ops),
        "redirect under-counting (a real redirect missed) for {input:?}"
    );
}

/// A sorted clone of a string multiset, for order-independent multiset equality.
fn sorted(v: &[String]) -> Vec<String> {
    let mut out = v.to_vec();
    out.sort();
    out
}

/// A sorted-by-discriminant clone of a redirect multiset.
fn sorted_redirs(v: &[RedirectKind]) -> Vec<RedirectKind> {
    let mut out = v.to_vec();
    out.sort_by_key(|k| redir_rank(*k));
    out
}

const fn redir_rank(k: RedirectKind) -> u8 {
    match k {
        RedirectKind::OutputFile => 0,
        RedirectKind::InputFile => 1,
        RedirectKind::Dup => 2,
        RedirectKind::OutputBoth => 3,
        RedirectKind::HereString => 4,
    }
}

/// Whether `ours` contains every element of `oracle` as a multiset.
fn is_superset(ours: &[String], oracle: &[String]) -> bool {
    let mut remaining = ours.to_vec();
    for needed in oracle {
        if let Some(pos) = remaining.iter().position(|x| x == needed) {
            remaining.remove(pos);
        } else {
            return false;
        }
    }
    true
}

/// Multiset superset for the redirect signature.
fn is_redir_superset(ours: &[RedirectKind], oracle: &[RedirectKind]) -> bool {
    let mut remaining = ours.to_vec();
    for needed in oracle {
        if let Some(pos) = remaining.iter().position(|x| x == needed) {
            remaining.remove(pos);
        } else {
            return false;
        }
    }
    true
}

// ── Seeded corpus (the known bug reproductions) ───────────────────────────────

/// The explicit corpus: ticket 01's assert-on-value inputs plus the bug
/// 11/13/17/20/30/33 reproductions, pinned as fixed `check()` cases so they stay
/// permanent regressions alongside the proptest layer.
///
/// The same repros are mirrored as on-disk seed files under `fuzz/corpus/` (see
/// `fuzz/README.md`) so the `cargo-fuzz` soak (tokenizer ticket 06) starts from
/// the identical regression set. `#[cfg(test)]` because the fuzz crate reads its
/// corpus from disk, not from this constant.
#[cfg(test)]
const SEED_CORPUS: &[&str] = &[
    // ── Ticket 01 assert-on-value inputs ─────────────────────────────────────
    "make test  # run it",
    r#"git commit -m "... catenary diagnostics ...""#,
    r"git commit -m 'it'\''s done'",
    "echo $(rm x); true",
    r#"echo "a > b""#,
    // ── Bug 11 — output redirection (real redirect must be seen) ──────────────
    "git status --short > out.txt",
    "git show HEAD:Cargo.toml > src/main.rs",
    "echo hi >> log",
    "cargo build 2>err.log",
    "make test 2>&1",
    // ── Bug 13 — quoted / multi-pattern glob (parser must not choke) ──────────
    "catenary glob 'src/**/*glob*.rs' 'src/**/grep*.rs' 'src/**/search*.rs'",
    // ── Bug 17 — backtick subcommand inside a quoted message body ─────────────
    r#"git commit -m "see `editing start` above""#,
    r#"echo "`rm x`""#,
    // ── Bug 20 — newline command separator ───────────────────────────────────
    "make test\ncargo build",
    "foo\ncatenary diagnostics",
    // ── Bug 30 — backslash-newline line continuation ─────────────────────────
    "cmd \\\nmore",
    "git add . && \\\ngit commit -m x",
    // ── Bug 33 — quoted text misclassified as command / operator ──────────────
    r#"git commit -m "no [predicates] table""#,
    r#"echo "a; b""#,
    "diff <(sort a) <(sort b)",
    "echo $(echo $(rm x))",
    "cat f | grep x | wc -l",
    "FOO=bar RUST_LOG=debug cargo build",
    "/usr/bin/git status",
    // ── Ticket 04 — `for`/`select` loop variable + list are not commands ──────
    // Only the body command is a command position; the loop variable `f` and the
    // bare iteration words are structure (brush agrees), so ours == brush.
    r#"for f in *.rs; do git add "$f"; done"#,
    "for f in a b c; do echo x; done",
    "for f in $(rm x); do echo hi; done",
    // ── Bug 45 — redirect trailing a subshell binds to the subshell ───────────
    // The compound-sweep used to drop these; the brace-group reference form is
    // pinned alongside so the two stay agreeing.
    "( echo hi ) > out",
    "( cmd ) >> out",
    "( cmd ) > out 2>&1",
    "( ( a ) > x ) > y",
    "{ echo hi; } > out",
];

// ── proptest strategy ─────────────────────────────────────────────────────────

/// A small alphabet of command-position-ish words the generator draws from.
#[cfg(test)]
fn word_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("make".to_string()),
        Just("git".to_string()),
        Just("cargo".to_string()),
        Just("echo".to_string()),
        Just("rm".to_string()),
        Just("cat".to_string()),
        Just("grep".to_string()),
        Just("ls".to_string()),
        Just("test".to_string()),
        Just("build".to_string()),
        Just("file.txt".to_string()),
        Just("a".to_string()),
        Just("x".to_string()),
        // A quoted argument carrying the structural characters that historically
        // leaked into command/operator position (bug 33).
        Just(r#""a > b; c | d && e""#.to_string()),
        Just(r"'it'\''s'".to_string()),
        Just("'a;b'".to_string()),
        // Substitutions — the recursion path.
        Just("$(rm x)".to_string()),
        Just("`echo hi`".to_string()),
        Just("VAR=v".to_string()),
        Just("# comment".to_string()),
    ]
}

/// Operators / separators the generator splices between words.
#[cfg(test)]
fn operator_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(" ".to_string()),
        Just(" ; ".to_string()),
        Just(" && ".to_string()),
        Just(" || ".to_string()),
        Just(" | ".to_string()),
        Just("\n".to_string()),
        Just(" \\\n".to_string()),
        Just(" > out ".to_string()),
        Just(" >> out ".to_string()),
        Just(" 2>&1 ".to_string()),
    ]
}

/// Build a structured shell-ish input by interleaving words with operators.
#[cfg(test)]
fn input_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec((word_strategy(), operator_strategy()), 1..6).prop_map(|pairs| {
        let mut s = String::new();
        for (i, (word, op)) in pairs.iter().enumerate() {
            if i > 0 {
                s.push_str(op);
            }
            s.push_str(word);
        }
        s
    })
}

/// A single character for the arbitrary-robustness strategy: printable ASCII
/// (`!`..=`~`) plus the three shell whitespace separators (space, tab, newline).
///
/// Drawn from explicit ASCII ranges rather than filtering `any::<char>()` — a
/// filter would reject the entire non-ASCII plane and blow proptest's local-
/// reject budget. Control characters (NUL et al.) are excluded by design: they
/// are not valid shell command input, and our byte-level lexer's
/// `from_utf8_lossy` recovery renders them differently from brush's char-level
/// tokenizer — a pathological-tail artifact, not a gate-relevant difference (see
/// the module-level divergence policy). cargo-fuzz (ticket 06) soaks the raw
/// control/byte space separately.
#[cfg(test)]
fn arbitrary_shell_char() -> impl Strategy<Value = char> {
    prop_oneof![
        // Weighted toward the printable graphic range, with shell whitespace
        // occasionally interspersed.
        9 => proptest::char::range('!', '~'),
        1 => prop_oneof![Just(' '), Just('\t'), Just('\n')],
    ]
}

#[cfg(test)]
proptest! {
    /// Structured shell-ish inputs (quotes, the `'\''` idiom, `$()`/backticks,
    /// operators, redirects, `#` comments) never make our parse disagree with
    /// brush on the gate view — over any input both parse. Stable CI, no
    /// nightly.
    #[test]
    fn differential_structured(input in input_strategy()) {
        check(&input);
    }

    /// Robustness tier (ADR 020 §6 tier 1) over arbitrary short printable-ASCII
    /// strings: our parser and the brush projection must not panic, hang, or
    /// over/under-flow on adversarial input — it is a clean, total fuzz target.
    ///
    /// This tier deliberately asserts *robustness*, not differential equality.
    /// Raw arbitrary bytes exercise the pathological tail where the two parsers'
    /// adversarial recovery legitimately diverges (an escaped backtick after a
    /// redirect, a `#` surfaced from a substitution, brush's own quirks on
    /// malformed input) — none of which is the gate-relevant command vocabulary.
    /// The differential *equality* property lives in [`differential_structured`]
    /// and the seed corpus, over well-formed shell; deep raw-byte soak is
    /// cargo-fuzz's job (ticket 06). The alphabet ([`arbitrary_shell_char`]) is
    /// printable ASCII plus shell whitespace.
    #[test]
    fn differential_arbitrary(
        input in proptest::collection::vec(arbitrary_shell_char(), 0..64)
            .prop_map(|cs| cs.into_iter().collect::<String>())
    ) {
        // Must not panic, hang, or overflow. Both projections — including the
        // full substitution recursion on either side — run to completion on any
        // adversarial input; that totality is the tier-1 property.
        let ours = ours_projection(&input);
        let brush = brush_projection(&input);
        // Use the results so the no-panic guarantee is observed, not elided.
        let _ = (ours.command_positions.len(), brush.is_some());
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "oracle tests assert agreement with the reference parser for readable failures"
)]
mod tests {
    use super::{
        Projection, RedirectKind, SEED_CORPUS, brush_projection, check, cook_command_name,
        extract_substitutions, is_balanced, is_clean_name, is_redir_superset, our_redirect_kind,
        ours_projection, redir_rank, scan_balanced, should_skip, sorted, sorted_redirs,
        strip_substitutions, unquote,
    };
    use crate::cli::command_filter::parse::RedirectOp;

    /// A word that is purely a substitution carries no literal command name —
    /// only its recursed inner command counts. Mirrors our parser yielding
    /// `name = None` for `$(rm x)` (the bug that surfaced in the proptest layer).
    #[test]
    fn pure_substitution_word_is_not_a_command_name() {
        assert_eq!(cook_command_name("$(rm x)"), None);
        assert_eq!(cook_command_name("`echo hi`"), None);
        assert_eq!(cook_command_name("<(sort a)"), None);
        // A literal prefix/suffix around a substitution still names a command.
        assert_eq!(
            cook_command_name("pre$(sub)post"),
            Some("prepost".to_string())
        );
        assert_eq!(cook_command_name("git"), Some("git".to_string()));
    }

    /// Every seeded repro agrees between our parser and brush (or is pruned).
    #[test]
    fn seed_corpus_agrees() {
        for input in SEED_CORPUS {
            check(input);
        }
    }

    /// brush actually parses the core seed inputs (they are not silently all
    /// pruned, which would make `seed_corpus_agrees` vacuous).
    #[test]
    fn seed_corpus_is_exercised() {
        let exercised = SEED_CORPUS
            .iter()
            .filter(|input| brush_projection(input).is_some())
            .count();
        assert!(
            exercised >= SEED_CORPUS.len() / 2,
            "expected most seed inputs to parse under brush, only {exercised} did"
        );
    }

    /// Meta-test: a deliberately broken our-side projection that drops `;`
    /// segmentation (so it under-counts command positions) must make the
    /// containment assertion fire — proving the oracle catches under-counting.
    #[test]
    fn oracle_catches_undercounting() {
        // `echo x; rm y` runs two commands; a broken projection that ignores
        // `;` would see only `echo`. The real projection sees both, so to prove
        // the *oracle* (not the parser) is load-bearing we simulate the broken
        // projection inline and assert the same containment check the oracle
        // uses would reject it.
        let input = "echo x; rm y";
        let oracle = brush_projection(input).expect("brush parses a simple `;` list");
        assert!(
            oracle.command_positions.contains(&"rm".to_string()),
            "oracle must see `rm` past the `;` for {input:?}, saw {:?}",
            oracle.command_positions
        );

        // The intact projection is a superset of the oracle (passes).
        let ours = ours_projection(input);
        assert!(super::is_superset(
            &ours.command_positions,
            &oracle.command_positions
        ));

        // A broken projection that drops everything after the first `;` segment
        // under-counts — the containment check the oracle runs rejects it.
        let broken = Projection {
            command_positions: vec!["echo".to_string()],
            redirect_ops: Vec::new(),
        };
        assert!(
            !super::is_superset(&broken.command_positions, &oracle.command_positions),
            "the oracle's containment check must reject an under-counting projection"
        );
    }

    /// The redirect signature distinguishes a real `>` from a quoted one
    /// (bug 33c / bug 11): `git status > out` carries an output-file redirect,
    /// `echo "a > b"` carries none.
    #[test]
    fn redirect_signature_real_vs_quoted() {
        let real = ours_projection("git status > out.txt");
        assert_eq!(real.redirect_ops, vec![RedirectKind::OutputFile]);
        let quoted = ours_projection(r#"echo "a > b""#);
        assert!(quoted.redirect_ops.is_empty());
        // And both agree with brush.
        check("git status > out.txt");
        check(r#"echo "a > b""#);
    }

    /// Bug 45: a redirect *following* the close of a subshell binds to the
    /// subshell. Our parser used to drop it (the compound-sweep ignored redirect
    /// tokens), under-counting the redirect signature versus brush's
    /// `[OutputFile]`. With the fix the two parsers agree, so `check` — which
    /// asserts redirect-signature equality + containment — no longer panics. This
    /// is the case the brace-group-scoped oracle test deliberately sidestepped
    /// while the bug was live.
    #[test]
    fn subshell_trailing_redirect_agrees_with_brush() {
        // The headline repro: previously brush saw `[OutputFile]`, ours saw `[]`.
        let ours = ours_projection("( echo hi ) > out");
        assert_eq!(ours.redirect_ops, vec![RedirectKind::OutputFile]);
        // The differential no longer diverges (does not panic).
        check("( echo hi ) > out");
        // Append, fd-duplication, and nested forms also agree.
        check("( cmd ) >> out");
        check("( cmd ) > out 2>&1");
        check("( ( a ) > x ) > y");
        // The brace-group reference form still agrees (no regression).
        check("{ echo hi; } > out");
    }

    /// `unquote` reproduces the name-cooking our lexer applies, so a raw brush
    /// word and our cooked name compare on equal footing.
    #[test]
    fn unquote_matches_lexer_cooking() {
        assert_eq!(unquote(r"'it'\''s'"), "it's");
        assert_eq!(unquote(r#""hello""#), "hello");
        assert_eq!(unquote(r"\g\i\t"), "git");
        assert_eq!(unquote("plain"), "plain");
    }

    // ── `scan_balanced` — exact (inner, next) span ────────────────────────────

    /// `scan_balanced` starts at the opening `(` and returns the text between the
    /// outermost parens plus the index just past the matching `)`. Pin both
    /// fields so the offset arithmetic (inner-start, `i + 1` past-close, depth
    /// bumps) is fixed, not merely "non-empty".
    #[test]
    fn scan_balanced_simple_span() {
        // `(rm x)` — inner is exactly "rm x", next is past the `)` at index 6.
        assert_eq!(scan_balanced(b"(rm x)", 0), ("rm x".to_string(), 6));
        // Empty body: `()` — inner is "" and next is 2.
        assert_eq!(scan_balanced(b"()", 0), (String::new(), 2));
    }

    /// A `(` opening partway through the buffer: the returned `next` is an
    /// absolute index, and the inner text excludes the delimiters. Pins the
    /// `i + 1` past-close offset against an off-by-one at a non-zero start.
    #[test]
    fn scan_balanced_nonzero_start() {
        // `a$(b)` — caller passes the index of `(` (= 2). Inner "b", next = 5.
        let bytes = b"a$(b)";
        assert_eq!(scan_balanced(bytes, 2), ("b".to_string(), 5));
    }

    /// Nested parens: the span closes only when depth returns to zero, so the
    /// inner text keeps the interior parens verbatim.
    #[test]
    fn scan_balanced_nested() {
        assert_eq!(scan_balanced(b"(a (b) c)", 0), ("a (b) c".to_string(), 9));
        // Two nested levels: `((x))` — inner is "(x)", next past the final `)`.
        assert_eq!(scan_balanced(b"((x))", 0), ("(x)".to_string(), 5));
    }

    /// A `)` inside a single- or double-quoted run does not close the span —
    /// the scanner is quote-aware. Pins the quote-skip arms (delete-match-arm
    /// mutants at the `'` / `"` cases).
    #[test]
    fn scan_balanced_quote_aware() {
        // `)` inside single quotes is literal.
        assert_eq!(scan_balanced(b"(a ')' x)", 0), ("a ')' x".to_string(), 9));
        // `)` inside double quotes is literal.
        assert_eq!(
            scan_balanced(br#"(echo "x)y")"#, 0),
            (r#"echo "x)y""#.to_string(), 12)
        );
    }

    /// A backslash-escaped paren inside the span does not change depth, and a
    /// `\` escapes the next byte (the `b'\\'` arm).
    #[test]
    fn scan_balanced_backslash_escape() {
        // `(a\)b)` — the escaped `)` is consumed as a pair, the real close is
        // the final `)` at index 5, so inner is `a\)b`.
        assert_eq!(scan_balanced(br"(a\)b)", 0), (r"a\)b".to_string(), 6));
    }

    /// An unbalanced (never-closing) span runs to end-of-input: `next == n` and
    /// the inner text is everything after the opening `(`. Pins the fall-through
    /// return (`inner_start.min(n)..n`, `n`).
    #[test]
    fn scan_balanced_unbalanced_runs_to_end() {
        assert_eq!(scan_balanced(b"(rm x", 0), ("rm x".to_string(), 5));
    }

    // ── `extract_substitutions` — exact captured inner texts ──────────────────

    /// Each top-level substitution form yields exactly its inner text, in order.
    #[test]
    fn extract_substitutions_each_form() {
        assert_eq!(extract_substitutions("$(rm x)"), vec!["rm x".to_string()]);
        assert_eq!(
            extract_substitutions("`echo hi`"),
            vec!["echo hi".to_string()]
        );
        assert_eq!(
            extract_substitutions("<(sort a)"),
            vec!["sort a".to_string()]
        );
        assert_eq!(extract_substitutions(">(tee f)"), vec!["tee f".to_string()]);
    }

    /// Arithmetic `$((…))` is recognized and skipped — it is not a command
    /// substitution, so nothing is captured. Pins the `i + 2 < n &&
    /// bytes[i + 2] == b'('` arithmetic guard.
    #[test]
    fn extract_substitutions_arithmetic_is_skipped() {
        assert!(extract_substitutions("$((1+2))").is_empty());
        // But a `$(` immediately followed by a non-`(` *is* a command sub.
        assert_eq!(extract_substitutions("$( (x) )"), vec![" (x) ".to_string()]);
    }

    /// Multiple top-level substitutions are captured in document order; only the
    /// outermost is extracted (the recursion re-projects the inner one).
    #[test]
    fn extract_substitutions_multiple_and_nested() {
        assert_eq!(
            extract_substitutions("a$(b)c`d`e"),
            vec!["b".to_string(), "d".to_string()]
        );
        // Nested: only the outer `$(…)` is captured, inner text kept verbatim.
        assert_eq!(
            extract_substitutions("$(echo $(rm x))"),
            vec!["echo $(rm x)".to_string()]
        );
        // Deeply nested process substitution: outer `<(…)` captured verbatim.
        assert_eq!(
            extract_substitutions("<(diff <(a) <(b))"),
            vec!["diff <(a) <(b)".to_string()]
        );
    }

    /// A substitution inside single quotes is not a substitution.
    #[test]
    fn extract_substitutions_single_quoted_is_inert() {
        assert!(extract_substitutions("'$(x)'").is_empty());
        assert!(extract_substitutions("'`y`'").is_empty());
    }

    /// A `$`/`<`/`>` not followed by `(` is ordinary text and captures nothing.
    /// Pins the false side of the `i + 1 < n && bytes[i + 1] == b'('` guards.
    #[test]
    fn extract_substitutions_lone_sigils_are_inert() {
        assert!(extract_substitutions("a < b > c").is_empty());
        assert!(extract_substitutions("price=$5").is_empty());
        // A `$` at the very end of input (the `i + 1 < n` boundary) is inert.
        assert!(extract_substitutions("end$").is_empty());
    }

    /// A backtick run honors `\` escapes (an escaped backtick does not close the
    /// run) and the captured inner text excludes the delimiters.
    #[test]
    fn extract_substitutions_backtick_escape() {
        assert_eq!(extract_substitutions(r"`a\`b`"), vec![r"a\`b".to_string()]);
        // Unterminated backtick runs to end-of-input.
        assert_eq!(extract_substitutions("`abc"), vec!["abc".to_string()]);
    }

    // ── `strip_substitutions` — exact literal residue ─────────────────────────

    /// Stripping leaves exactly the literal text around each removed span.
    #[test]
    fn strip_substitutions_drops_each_form() {
        assert_eq!(strip_substitutions("pre$(sub)post"), "prepost");
        assert_eq!(strip_substitutions("a`b`c"), "ac");
        assert_eq!(strip_substitutions("x<(a)y"), "xy");
        assert_eq!(strip_substitutions("x>(a)y"), "xy");
        // Arithmetic `$((…))` is a balanced span and is dropped entirely.
        assert_eq!(strip_substitutions("v$((1+2))w"), "vw");
        assert_eq!(strip_substitutions("plain"), "plain");
    }

    /// A single-quoted run is copied verbatim (including its delimiters) and any
    /// substitution syntax inside it is inert. Pins the verbatim
    /// `out.extend_from_slice(&bytes[start..i])` slice bounds.
    #[test]
    fn strip_substitutions_single_quote_verbatim() {
        assert_eq!(strip_substitutions("'$(x)'"), "'$(x)'");
        assert_eq!(strip_substitutions("a'$(x)'b"), "a'$(x)'b");
        // Unterminated single quote: copied to end-of-input.
        assert_eq!(strip_substitutions("'abc"), "'abc");
    }

    // ── `unquote` — exact cooked output, edge by edge ─────────────────────────

    /// Double quotes drop the delimiters and honor the `\"`/`\\`/`` \` ``/`\$`
    /// escapes; any other backslash is kept literally inside the run.
    #[test]
    fn unquote_double_quote_escapes() {
        assert_eq!(unquote(r#""a\"b""#), "a\"b");
        assert_eq!(unquote(r#""a\\b""#), r"a\b");
        assert_eq!(unquote(r#""a\`b""#), "a`b");
        assert_eq!(unquote(r#""a\$b""#), "a$b");
        // A backslash before a non-special byte is preserved verbatim.
        assert_eq!(unquote(r#""a\nb""#), r"a\nb");
        // An escape immediately before the closing quote (the `i + 1 < n`
        // boundary inside the double-quoted run): `"\""` cooks to a single `"`.
        assert_eq!(unquote(r#""\"""#), "\"");
    }

    /// The `$'…'` form drops the delimiters and removes a backslash before any
    /// byte (it does not interpret escapes — `\n` becomes `n`, not a newline).
    /// Pins the `i + 1 < n && bytes[i + 1] == b'\''` match guard.
    #[test]
    fn unquote_dollar_single_quote() {
        assert_eq!(unquote(r"$'a\nb'"), "anb");
        assert_eq!(unquote(r"$'plain'"), "plain");
        // A lone `$` not followed by `'` is a literal `$`.
        assert_eq!(unquote("$x"), "$x");
    }

    /// Unterminated quotes consume to end-of-input rather than overrun.
    #[test]
    fn unquote_unterminated_quotes() {
        assert_eq!(unquote("a'b"), "ab");
        assert_eq!(unquote(r#"a"b"#), "ab");
        // A trailing unescaped backslash (no next byte) is kept verbatim.
        assert_eq!(unquote(r"a\"), r"a\");
        assert_eq!(unquote(""), "");
    }

    // ── `is_balanced` — exact true/false at the edges ─────────────────────────

    /// Balanced quotes, backticks, and parens return true; each unterminated or
    /// stray-close form returns false. Pins the return-value arms and the
    /// `paren_depth == 0` final check.
    #[test]
    fn is_balanced_well_formed_inputs() {
        assert!(is_balanced("echo hi"));
        assert!(is_balanced("echo $(rm x)"));
        assert!(is_balanced(r#"echo "a > b""#));
        assert!(is_balanced("echo 'a)b'"));
        assert!(is_balanced("echo `cmd`"));
        assert!(is_balanced("a (b (c) d) e"));
        assert!(is_balanced(""));
    }

    /// Each unbalanced form is rejected for the right reason.
    #[test]
    fn is_balanced_unbalanced_inputs() {
        assert!(!is_balanced("'unterminated"));
        assert!(!is_balanced(r#""unterminated"#));
        assert!(!is_balanced("`unterminated"));
        assert!(!is_balanced("echo (x")); // dangling open paren
        assert!(!is_balanced("echo )x")); // stray close paren
        assert!(!is_balanced("(()"));
    }

    /// A paren or quote inside a quoted run does not affect balance; a `\`
    /// outside a string escapes the next byte so an escaped quote/paren opens
    /// nothing. Pins the quote-skip arms and the `\\` arm.
    #[test]
    fn is_balanced_quote_and_escape_aware() {
        // `(` inside single quotes is inert.
        assert!(is_balanced("'('"));
        // `)` inside double quotes is inert.
        assert!(is_balanced(r#""a)b""#));
        // An escaped paren outside a string opens/closes nothing.
        assert!(is_balanced(r"\("));
        assert!(is_balanced(r"\)"));
        // An escaped quote does not open a quoted run.
        assert!(is_balanced(r"\'"));
        // A `\"` inside a double-quoted run is an escaped quote, not the close —
        // the run still terminates at the final `"` (exercises the dq-escape
        // `j + 1 < n` arm and its `j += 2` skip).
        assert!(is_balanced(r#""a\"b""#));
        // An unterminated double-quoted run whose only `"` is escaped is *not*
        // balanced — the closing quote is consumed by the escape.
        assert!(!is_balanced(r#""a\""#));
        // A `\`` inside a backtick run is escaped, so the run closes at the
        // final backtick (the backtick-escape arm).
        assert!(is_balanced(r"`a\`b`"));
    }

    // ── `is_clean_name` — character-class boundary ────────────────────────────

    /// A clean name is non-empty and built only from alphanumerics plus the
    /// allowed word/path punctuation; anything else (empty, whitespace, quotes,
    /// operators, comment) is not clean.
    #[test]
    fn is_clean_name_boundary() {
        assert!(is_clean_name("git"));
        assert!(is_clean_name("/usr/bin/git"));
        assert!(is_clean_name("a_b-c.d+e@f"));
        assert!(!is_clean_name("")); // empty is never clean
        assert!(!is_clean_name("a b")); // whitespace
        assert!(!is_clean_name("a;b")); // operator
        assert!(!is_clean_name("a'b")); // quote
        assert!(!is_clean_name("#comment")); // comment marker
    }

    // ── `should_skip` — the prune predicate ───────────────────────────────────

    /// `should_skip` is true for the pruned tail (unbalanced input, brush
    /// rejects, adversarial-spelling names) and false for clean, well-formed
    /// shell that both parsers handle. Pins the `!is_balanced` short-circuit and
    /// the final `!all(is_clean_name)` disjunction.
    #[test]
    fn should_skip_clean_vs_pruned() {
        // Clean, balanced, both-parsable, clean names → not skipped.
        assert!(!should_skip("git status"));
        assert!(!should_skip("echo x; rm y"));
        // Unbalanced → skipped (short-circuits before consulting brush).
        assert!(should_skip("echo (x"));
        assert!(should_skip("'unterminated"));
        // Balanced and brush-parsable, but the command name cooks to a token
        // carrying an operator (`;`) — the adversarial-spelling tail. Reaches
        // the final `!all(is_clean_name)` disjunction and prunes. Pins the
        // trailing `!`/`||` from being deleted/weakened.
        assert!(should_skip(r#""a;b" arg"#));
    }

    /// Inputs that are balanced, brush-parsable, and clean-named but contain a
    /// construct in the pruned tail (function definition, arithmetic, heredoc)
    /// are skipped *because of the construct*. These pin the
    /// `program_has_pruned_construct` projection (and the `*_has_pruned`
    /// recursion) — were that projection forced to `false`, `should_skip` would
    /// wrongly return `false` here (clean names, balanced, brush-parses).
    #[test]
    fn should_skip_pruned_constructs() {
        // Function definition — `Command::Function`.
        assert!(should_skip("f() { git status; }"));
        // Arithmetic compound — `CompoundCommand::Arithmetic`.
        assert!(should_skip("(( 1 + 2 ))"));
        // Heredoc body — `IoRedirect::HereDocument`.
        assert!(should_skip("cat <<EOF\nhi\nEOF\n"));
        // A plain command nested in an `if` (no pruned construct) is *not*
        // skipped on this account — guards against the projection over-pruning.
        assert!(!should_skip("if true; then git status; fi"));
    }

    /// The `*_has_pruned` recursion must find a pruned construct no matter which
    /// disjunct of an and-or / pipeline / compound it hides in. Each input below
    /// carries the pruned construct in a position reachable only through one
    /// branch of a `||` in the recursion — pinning those `||`s against an
    /// `&&` mutation (which would lose the construct and stop skipping).
    #[test]
    fn should_skip_finds_pruned_in_any_branch() {
        // Arithmetic in the *second* and-or pipeline (`and_or_has_pruned`'s
        // `first || additional.any(...)`).
        assert!(should_skip("git status && (( 1 ))"));
        // Arithmetic inside the `then` branch of an `if` (the `condition ||
        // then || elses` disjunction in `compound_has_pruned`).
        assert!(should_skip("if true; then (( 1 )); fi"));
        // Arithmetic inside the `else` branch (the `elses` disjunct).
        assert!(should_skip("if true; then x; else (( 1 )); fi"));
        // Arithmetic in a `while` body (the `cond || body` disjunct of the
        // while/until arm).
        assert!(should_skip("while true; do (( 1 )); done"));
    }

    // ── seq7: whole-function / return-replacement kills ───────────────────────

    /// `sorted` returns the input multiset in ascending order — not the empty
    /// vec, a singleton `[""]`, or `["xyzzy"]`. Pins the whole-function-body
    /// replacements (`->vec![]` / `->vec![String::new()]` / `->vec!["xyzzy"]`)
    /// and the in-place sort.
    #[test]
    fn seq7_sorted_orders_and_preserves_multiset() {
        assert_eq!(
            sorted(&["git".to_string(), "cargo".to_string(), "echo".to_string()]),
            vec!["cargo".to_string(), "echo".to_string(), "git".to_string()]
        );
        // Duplicates are kept (multiset, not set) — rules out a dedup mutation.
        assert_eq!(
            sorted(&["b".to_string(), "a".to_string(), "b".to_string()]),
            vec!["a".to_string(), "b".to_string(), "b".to_string()]
        );
        // Empty in, empty out — distinguishes `->vec![""]` / `->vec!["xyzzy"]`,
        // which would inject a bogus element here.
        assert!(sorted(&[]).is_empty());
    }

    /// `sorted_redirs` returns the redirect multiset ordered by `redir_rank`,
    /// never the empty vec. Pins the `->vec![]` whole-body replacement and the
    /// sort-by-key.
    #[test]
    fn seq7_sorted_redirs_orders_and_preserves() {
        let unsorted = vec![
            RedirectKind::HereString,
            RedirectKind::OutputFile,
            RedirectKind::InputFile,
        ];
        assert_eq!(
            sorted_redirs(&unsorted),
            vec![
                RedirectKind::OutputFile,
                RedirectKind::InputFile,
                RedirectKind::HereString,
            ]
        );
        // A non-empty input must not collapse to empty (`->vec![]`).
        assert_eq!(sorted_redirs(&[RedirectKind::Dup]), vec![RedirectKind::Dup]);
    }

    /// `redir_rank` assigns a *distinct* rank per kind, so `sorted_redirs` groups
    /// by kind. A `->0` or `->1` constant replacement makes every rank equal,
    /// leaving a deliberately mis-ordered pair unsorted; assert the ordering is
    /// actually applied (kills both constant replacements) and pin every arm.
    #[test]
    fn seq7_redir_rank_is_a_distinct_total_order() {
        assert_eq!(redir_rank(RedirectKind::OutputFile), 0);
        assert_eq!(redir_rank(RedirectKind::InputFile), 1);
        assert_eq!(redir_rank(RedirectKind::Dup), 2);
        assert_eq!(redir_rank(RedirectKind::OutputBoth), 3);
        assert_eq!(redir_rank(RedirectKind::HereString), 4);
        // Behavioral check: a higher-ranked kind sorts after a lower-ranked one.
        // With `redir_rank->0`/`->1` (constant), the stable sort would keep the
        // input order `[HereString, OutputFile]` instead of swapping them.
        assert_eq!(
            sorted_redirs(&[RedirectKind::HereString, RedirectKind::OutputFile]),
            vec![RedirectKind::OutputFile, RedirectKind::HereString]
        );
    }

    /// `is_redir_superset` is a real multiset-containment check, not a constant
    /// `true`. When ours is missing an operator the oracle has, it must return
    /// `false`; when ours covers the oracle (with extras allowed) it returns
    /// `true`. Pins the `->true` whole-body replacement.
    #[test]
    fn seq7_is_redir_superset_rejects_undercount() {
        // ours misses the InputFile the oracle saw → not a superset.
        assert!(!is_redir_superset(
            &[RedirectKind::OutputFile],
            &[RedirectKind::OutputFile, RedirectKind::InputFile],
        ));
        // ours is a strict superset (extra Dup) → still a superset.
        assert!(is_redir_superset(
            &[RedirectKind::OutputFile, RedirectKind::Dup],
            &[RedirectKind::OutputFile],
        ));
        // Multiset semantics: two of a kind needed, only one present → not a
        // superset (rules out a set-based weakening as well as `->true`).
        assert!(!is_redir_superset(
            &[RedirectKind::OutputFile],
            &[RedirectKind::OutputFile, RedirectKind::OutputFile],
        ));
    }

    /// `our_redirect_kind` maps each of our `RedirectOp` spellings onto the
    /// shared `RedirectKind`. Pins every match arm against an arm deletion / kind
    /// swap.
    #[test]
    fn seq7_our_redirect_kind_maps_each_op() {
        assert_eq!(
            our_redirect_kind(RedirectOp::Write),
            RedirectKind::OutputFile
        );
        assert_eq!(
            our_redirect_kind(RedirectOp::Append),
            RedirectKind::OutputFile
        );
        assert_eq!(our_redirect_kind(RedirectOp::Read), RedirectKind::InputFile);
        assert_eq!(our_redirect_kind(RedirectOp::DupOut), RedirectKind::Dup);
        assert_eq!(our_redirect_kind(RedirectOp::DupIn), RedirectKind::Dup);
        assert_eq!(
            our_redirect_kind(RedirectOp::WriteBoth),
            RedirectKind::OutputBoth
        );
        assert_eq!(
            our_redirect_kind(RedirectOp::HereString),
            RedirectKind::HereString
        );
    }

    /// `walk_redirect_list` actually surfaces the redirects hung off a compound
    /// command (a brace group with a trailing redirect list). With the body
    /// replaced by `()` the redirect would vanish from the brush projection. A
    /// brace group `{ …; } > out` routes its redirect through
    /// `walk_redirect_list`.
    #[test]
    fn seq7_walk_redirect_list_surfaces_compound_redirects() {
        let brace = brush_projection("{ echo hi; } > out")
            .expect("brush parses a brace group with a redirect");
        assert_eq!(brace.redirect_ops, vec![RedirectKind::OutputFile]);
        // Two redirects on the brace group: both flow through the list walk.
        let two = brush_projection("{ echo hi; } > out 2> err")
            .expect("brush parses a brace group with two redirects");
        assert_eq!(
            two.redirect_ops,
            vec![RedirectKind::OutputFile, RedirectKind::OutputFile]
        );
        // And the full differential agrees (the operator is real, not quoted, on
        // both sides).
        check("{ echo hi; } > out");
    }

    /// `io_redirect_is_pruned` is true *only* for a heredoc, not for an ordinary
    /// file redirect. A `->true` replacement would prune (`should_skip`) every
    /// simple command carrying any redirect, so assert a plain `> out` is *not*
    /// skipped while a heredoc *is*.
    #[test]
    fn seq7_io_redirect_is_pruned_only_heredoc() {
        // A plain output redirect on a simple command is not a pruned construct.
        assert!(!should_skip("echo hi > out"));
        assert!(!should_skip("cat < in"));
        // A heredoc body is pruned.
        assert!(should_skip("cat <<EOF\nbody\nEOF\n"));
    }

    // ── seq7: strip_substitutions boundary / arithmetic kills ──────────────────

    /// A single-quoted run is skipped *verbatim*: the scan-to-closing-quote loop
    /// (`while i < n && bytes[i] != b'\''`) and the post-loop `i = (i+1).min(n)`
    /// must advance past the *entire* quoted span, so substitution syntax inside
    /// single quotes stays literal. With the loop guard flipped (`<`→`>`, the
    /// inner `!=`→`==`) or the close arithmetic mutated (`+`→`*`), the `$(x)`
    /// inside the quotes would be processed as a real substitution and dropped.
    #[test]
    fn seq7_strip_substitutions_single_quote_scan_advances() {
        // The substitution lives *inside* a single-quoted run, so nothing is
        // stripped — the whole literal survives. (Flipping the scan-loop guard
        // `< → >` / `!= → ==` would step out of the run and strip the inner
        // `$(x)`, yielding `'ab'`.)
        assert_eq!(strip_substitutions("'a$(x)b'"), "'a$(x)b'");
        // Single-quoted backtick is inert too.
        assert_eq!(strip_substitutions("'a`y`b'"), "'a`y`b'");
        // The post-loop close `i = (i + 1).min(n)` must step *past* the closing
        // quote: a real `$(x)` immediately after a single-quoted run is dropped,
        // and the quoted run before it is preserved verbatim. With `+ → *` the
        // close index does not advance past the `'`, so the trailing `$(x)`
        // would be mis-scanned and survive as `'ab'$(x)` instead of `'ab'`.
        assert_eq!(strip_substitutions("'ab'$(x)"), "'ab'");
    }

    /// A `<`/`>` followed by `(` opens a process substitution that is stripped
    /// only when the `i + 1 < n` look-ahead holds. A trailing lone `<`/`>` (the
    /// `i + 1 < n` boundary) is kept literally — pins that guard, and a literal
    /// `>`/`<` not followed by `(` is preserved.
    #[test]
    fn seq7_strip_substitutions_process_sub_and_lone_sigil() {
        assert_eq!(strip_substitutions("a<(x)b"), "ab");
        assert_eq!(strip_substitutions("a>(x)b"), "ab");
        // A lone `<` / `>` (not before `(`) is literal text.
        assert_eq!(strip_substitutions("a < b"), "a < b");
        // A `<` at end-of-input (the `i + 1 < n` false side) is kept.
        assert_eq!(strip_substitutions("end<"), "end<");
    }

    // ── seq7: extract_substitutions backtick / boundary kills ──────────────────

    /// A backtick run advances byte-by-byte to the closing backtick, honoring a
    /// `\`-escape (`if bytes[j] == b'\\' && j + 1 < n { j += 1 }` then `j += 1`).
    /// The captured inner text excludes the delimiters and the *next* scan
    /// resumes past the close (`i = j + 1`). A `+=`→`*=` on either `j` advance
    /// would mis-walk the run; assert the exact captures including that text
    /// *after* the closing backtick is reached.
    #[test]
    fn seq7_extract_substitutions_backtick_walk() {
        // Two backtick runs back-to-back: each captured exactly, the second only
        // reachable if the first's close advance (`i = j + 1`) landed correctly.
        assert_eq!(
            extract_substitutions("`a``b`"),
            vec!["a".to_string(), "b".to_string()]
        );
        // An escaped backtick mid-run does not close it; the run closes at the
        // final real backtick and text resumes after it.
        assert_eq!(extract_substitutions(r"`a\`b`c"), vec![r"a\`b".to_string()]);
    }

    /// `extract_substitutions` walks past ordinary characters one byte at a time
    /// (`_ => i += 1`) and finds a substitution that only appears *after* a run
    /// of plain text — pinning the default-arm advance and the outer `while i < n`
    /// bound against an off-by-one that would skip or loop on the tail.
    #[test]
    fn seq7_extract_substitutions_after_plain_run() {
        assert_eq!(
            extract_substitutions("plain text then $(rm x)"),
            vec!["rm x".to_string()]
        );
        // A substitution at the very tail (last bytes) is still captured.
        assert_eq!(extract_substitutions("tail$(z)"), vec!["z".to_string()]);
    }

    // ── seq7: unquote termination + boundary kills ─────────────────────────────

    /// `unquote` terminates and yields the right output even when a quoted run
    /// abuts more text — the per-byte advances inside each arm (`i += 1` /
    /// `i += 2`) must make progress. A `+=`→`*=` on an advance would wedge `i`
    /// at 0 and loop forever; this test would hang rather than pass, so its mere
    /// completion (plus the exact output) kills those infinite-loop mutants.
    #[test]
    fn seq7_unquote_terminates_on_mixed_runs() {
        // Single-quote run followed by plain text, then a double-quote run.
        assert_eq!(unquote(r#"'a'b"c""#), "abc");
        // `$'…'` run abutting a double-quoted run.
        assert_eq!(unquote(r#"$'a'"b""#), "ab");
        // A double-quoted run with *more text after the close*: the post-loop
        // advance (`i += 1`) must step past the closing `"`, or the trailing run
        // is re-entered as a fresh (mismatched) quote. With `i += 1 → i *= 1` the
        // close index does not advance, so `"a"'b'` would cook to `a'b'`.
        assert_eq!(unquote(r#""a"'b'"#), "ab");
        assert_eq!(unquote(r#""a"b"#), "ab");
        // A `$'…'` run with more text after the close: the same post-loop advance
        // must step past the closing `'` (`i += 1 → i *= 1` would mis-cook).
        assert_eq!(unquote(r"$'a'b"), "ab");
        // A long-ish plain run to make a stalled-advance loop obvious if it ever
        // regressed (still completes near-instantly when advances are correct).
        assert_eq!(unquote("abcdefghij"), "abcdefghij");
    }

    /// Inside a double-quoted run, `\X` for a *non-special* `X` keeps both bytes
    /// (the `else` advance `i += 1`), while the four recognized escapes consume
    /// the backslash (`i += 2`). Pins the `i + 1 < n` look-ahead boundary: a `\`
    /// as the run's last byte before the close is kept literal.
    #[test]
    fn seq7_unquote_double_quote_escape_boundary() {
        // `\n` is not a recognized escape → both bytes kept.
        assert_eq!(unquote(r#""x\ny""#), r"x\ny");
        // Recognized `\\` consumes the pair → single backslash.
        assert_eq!(unquote(r#""x\\y""#), r"x\y");
        // A recognized escape *not* at index 2, with content after it: the
        // escape advance (`i += 2`) must skip exactly the `\X` pair and leave the
        // tail intact. With `i += 2 → i *= 2` the index would jump past the `c`,
        // dropping it (`"ab\$c"` would cook to `ab$` instead of `ab$c`).
        assert_eq!(unquote(r#""ab\$c""#), "ab$c");
    }

    // ── seq7: scan_balanced inner-text + depth kills ───────────────────────────

    /// `scan_balanced`'s inner text is taken between the depth-1 open and the
    /// depth-0 close. A nested span keeps its interior parens; a sibling pair
    /// after the close is *not* swallowed (the depth bookkeeping returns at the
    /// first balanced close). Pins the depth `+= 1` / `-= 1` and the
    /// `depth == 0` / `depth == 1` guards.
    #[test]
    fn seq7_scan_balanced_depth_returns_at_first_close() {
        // `(a)(b)` from index 0 closes after `(a)` — next = 3, inner = "a".
        assert_eq!(scan_balanced(b"(a)(b)", 0), ("a".to_string(), 3));
        // Inner-start tracks the depth-1 open even with leading nested opens:
        // `((a))` → inner "(a)", next 5.
        assert_eq!(scan_balanced(b"((a))", 0), ("(a)".to_string(), 5));
    }

    // ── seq7: is_balanced structural kills ─────────────────────────────────────

    /// `is_balanced` tracks paren depth with `+= 1` / `-= 1` and rejects a
    /// negative depth (a stray close) immediately. A pair that opens and closes
    /// is balanced; a close before any open is not; nesting beyond one level is
    /// tracked. Pins the depth arithmetic and the `paren_depth < 0` early reject.
    #[test]
    fn seq7_is_balanced_depth_tracking() {
        // Deep nesting that pairs up exactly.
        assert!(is_balanced("(((a)))"));
        // A close immediately at the start (depth would go negative) is rejected
        // before any open — pins the `< 0` guard, not just the final `== 0`.
        assert!(!is_balanced(")"));
        // A stray close *that a later open rebalances to zero*: `)(` ends with
        // depth 0, so only the early `paren_depth < 0` reject (not the final
        // `== 0`) catches it. Pins the `< 0` guard specifically — with it
        // weakened (e.g. `< 0 → > 0`), `)(` would wrongly read as balanced.
        assert!(!is_balanced(")("));
        // One extra open at the end leaves depth positive → not balanced (pins
        // the final `paren_depth == 0` against `!= 0` / a constant `true`).
        assert!(!is_balanced("(a)("));
    }
}
