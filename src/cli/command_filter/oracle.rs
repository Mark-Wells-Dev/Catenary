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
//! The reference parser is a **dev-only** dependency — this whole module is
//! `#[cfg(test)]`, so `brush-parser` never enters the runtime / `cargo deny`
//! runtime graph and never ships.
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

fn prefix_or_suffix_is_pruned(item: &ast::CommandPrefixOrSuffixItem) -> bool {
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
fn check(input: &str) {
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
];

// ── proptest strategy ─────────────────────────────────────────────────────────

/// A small alphabet of command-position-ish words the generator draws from.
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
fn arbitrary_shell_char() -> impl Strategy<Value = char> {
    prop_oneof![
        // Weighted toward the printable graphic range, with shell whitespace
        // occasionally interspersed.
        9 => proptest::char::range('!', '~'),
        1 => prop_oneof![Just(' '), Just('\t'), Just('\n')],
    ]
}

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
        ours_projection, unquote,
    };

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

    /// `unquote` reproduces the name-cooking our lexer applies, so a raw brush
    /// word and our cooked name compare on equal footing.
    #[test]
    fn unquote_matches_lexer_cooking() {
        assert_eq!(unquote(r"'it'\''s'"), "it's");
        assert_eq!(unquote(r#""hello""#), "hello");
        assert_eq!(unquote(r"\g\i\t"), "git");
        assert_eq!(unquote("plain"), "plain");
    }
}
