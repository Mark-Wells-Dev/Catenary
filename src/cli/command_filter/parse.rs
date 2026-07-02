// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Faithful, hand-rolled shell parser core (`&str → ParsedScript`).
//!
//! A pure, deterministic shell parse that reproduces shell word-splitting and
//! command segmentation for the subset the command gate needs (decision 020).
//! It models *which words are commands*, *which operators/redirects are real*,
//! and *how the call is structured* — nothing more. It does **not** expand
//! globs, variables, or arithmetic, and it carries **no gate, allowlist,
//! redirect, or isolation policy** (those live in sibling tickets 02–04).
//!
//! The parse is quote-faithful (single, double, `$'…'`, and the `'\''`
//! close·escape·reopen idiom), strips inline `#` comments, joins `\`-newline
//! line continuations (bug 30), removes heredoc bodies (projecting an *unquoted*
//! body's command substitutions, which the shell expands and runs — bug 46), and
//! recurses into `$(…)` / `` `…` `` / `<(…)` / `>(…)` substitutions.
//!
//! No I/O, no global state — a clean fuzz target. The public surface is one
//! free function, [`parse`], plus the value types it returns.

// This is the parse substrate (ticket 01); the allowlist evaluator and the
// redirect guard read it in tickets 02–04. Until then it is exercised only by
// this module's unit tests, so the `pub(crate)` API and its supporting helpers
// have no in-crate caller yet — allow `dead_code` here rather than scatter
// per-item attributes that ticket 02 would immediately remove.
#![allow(
    dead_code,
    reason = "parse substrate landed ahead of its gate callers (tickets 02–04 wire it)"
)]
// The `pub(crate)` API is intentional: it is the crate-internal surface those
// tickets import. Inside this not-yet-public-reachable module the lint reads it
// as redundant, but `pub(crate)` is the correct eventual visibility.
#![allow(
    clippy::redundant_pub_crate,
    reason = "pub(crate) is the intended cross-module visibility for tickets 02–04"
)]

use std::path::Path;

use super::patterns::{ENV_VAR_RE, HEREDOC_MARKER_RE};

/// A fully parsed shell script: an ordered list of pipelines joined by the
/// list operators `;` `&&` `||` newline `&`.
///
/// The ordering is document order; the joining operators themselves are not
/// retained — with one exception the gate needs: a pipeline terminated by a
/// bare `&` is marked [`Pipeline::backgrounded`] (the catenary isolation /
/// output-ownership gate denies a backgrounded catenary command, whose output
/// would be dropped — ticket 04).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ParsedScript {
    /// The pipelines, in document order.
    pub(crate) pipelines: Vec<Pipeline>,
}

/// A pipeline: an ordered list of simple commands joined by `|`.
///
/// A bare command (no pipe) is a one-element pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Pipeline {
    /// The simple commands, in pipeline order (position 0 is the head).
    pub(crate) commands: Vec<SimpleCommand>,
    /// Whether this pipeline is terminated by a bare backgrounding `&`
    /// (`cmd &`). The joining operator is otherwise not retained; this single
    /// bit is what the catenary gate needs to deny a backgrounded invocation
    /// (ticket 04).
    pub(crate) backgrounded: bool,
    /// The list operator that terminated this pipeline (`None` for the final,
    /// unterminated one). The write resolver reads it to tell whether the
    /// *next* pipeline executes unconditionally (`;`/newline/`&`) or
    /// conditionally (`&&`/`||`) — a conditional `VAR=value` binding cannot be
    /// trusted for later write-target resolution (ws38 ticket 01).
    pub(crate) terminator: Option<ListOp>,
}

/// The kind of list operator that terminated a pipeline, as the write
/// resolver needs it: does the *following* pipeline always execute?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListOp {
    /// `;`, newline, or backgrounding `&` — the next pipeline always runs.
    Seq,
    /// `&&` — the next pipeline runs only on success.
    And,
    /// `||` — the next pipeline runs only on failure.
    Or,
}

/// A simple command: a command-position word, its arguments, redirections, and
/// any command substitutions appearing anywhere in its words.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct SimpleCommand {
    /// The command-position word, after skipping `VAR=value` prefixes and
    /// stripping the leading path (`/usr/bin/git` → `git`). `None` when the
    /// segment has no command word (only assignments, only redirects, or
    /// empty).
    pub(crate) name: Option<String>,
    /// The remaining words after the command word, in order.
    pub(crate) argv: Vec<String>,
    /// Expansion provenance for each `argv` word, index-parallel to
    /// [`Self::argv`]. Read by the write resolver (ws38 ticket 01).
    pub(crate) argv_meta: Vec<WordMeta>,
    /// The leading `VAR=value` assignment words (skipped when locating the
    /// command word), in order. The write resolver reads them to bind `$VAR`
    /// write targets appearing later in the same command line.
    pub(crate) assignments: Vec<Assignment>,
    /// The redirections attached to this command.
    pub(crate) redirects: Vec<Redirect>,
    /// Command substitutions (`$(…)`, `` `…` ``, `<(…)`, `>(…)`) found anywhere
    /// in this command's words, each recursively parsed.
    pub(crate) substitutions: Vec<ParsedScript>,
    /// Whether this segment is a compound command — wrapped by a reserved word
    /// (`for`/`while`/`until`/`if`/`case`/`{`) or a `(` subshell. The parser
    /// only *recognizes* compounds; policy is the caller's (ticket 04).
    pub(crate) is_compound: bool,
    /// The `for`/`select` loop variable, when this segment is a loop header.
    /// The write resolver taints it — its runtime value is per-iteration.
    pub(crate) loop_var: Option<String>,
}

/// A `VAR=value` assignment word, as the write resolver needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Assignment {
    /// The variable name (the part before `=`).
    pub(crate) name: String,
    /// The cooked value text (after `=`): quotes interpreted away, expansions
    /// *not* performed.
    pub(crate) value: String,
    /// Expansion provenance of the whole assignment word.
    pub(crate) meta: WordMeta,
}

/// Expansion provenance of a word, tracked per character channel by the lexer.
///
/// The write resolver (ws38 ticket 01) needs more than a word's cooked text:
/// it must know *which shell expansions are live* in it. An unquoted `$`,
/// glob, or brace expands at runtime; the same byte arriving through a quoted
/// or escaped channel is literal filename data. One flag per expansion family
/// keeps the resolver's fail-closed rules cheap — any ambiguity (a
/// literal-channel metacharacter alongside a live one) is visible as a flag
/// combination and classified opaque rather than guessed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one orthogonal provenance bit per expansion family; a state \
              machine would obscure the resolver's independent checks"
)]
pub(crate) struct WordMeta {
    /// A live (expanding) `$` reached the word: bare `$NAME`, or `$…` inside
    /// double quotes. Not set by `'$'`, `\$`, or `$'…'` (literal channels).
    pub(crate) live_dollar: bool,
    /// A live glob character (`*`, `?`, `[`) reached the word unquoted.
    pub(crate) live_glob: bool,
    /// A live `{` / `}` reached the word unquoted (candidate brace expansion).
    pub(crate) live_brace: bool,
    /// The word starts with a live (unquoted) `~` (candidate tilde expansion).
    pub(crate) live_tilde: bool,
    /// An expansion-capable metacharacter reached the word through a literal
    /// channel: single/ANSI-C quotes, a backslash escape, or a double-quoted
    /// glob/brace/tilde.
    pub(crate) literal_meta: bool,
    /// The word carried `$(…)` / `` `…` `` value substitutions (their output
    /// becomes word text at runtime).
    pub(crate) value_subs: bool,
    /// The word carried `<(…)` / `>(…)` process substitutions (a `/dev/fd`
    /// pipe path, not runtime text).
    pub(crate) process_subs: bool,
}

impl WordMeta {
    /// Whether any live (runtime) expansion is present in the word.
    pub(crate) const fn any_live(self) -> bool {
        self.live_dollar || self.live_glob || self.live_brace || self.live_tilde
    }
}

/// A redirection operator and its target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Redirect {
    /// The redirection operator.
    pub(crate) op: RedirectOp,
    /// The target word as written (path, `&1`, `/dev/null`, …). For an fd
    /// duplication (`2>&1`) this is the `&N` form; for a file redirect it is
    /// the target path. Empty when the operator had no following word.
    pub(crate) target: String,
    /// Expansion provenance of the target word (default for a missing word).
    pub(crate) target_meta: WordMeta,
}

/// The kind of a redirection operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RedirectOp {
    /// `>` — truncating output redirect.
    Write,
    /// `>>` — appending output redirect.
    Append,
    /// `<` — input redirect.
    Read,
    /// `>&` / `N>&` — output fd duplication / `>&word`.
    DupOut,
    /// `<&` / `N<&` — input fd duplication.
    DupIn,
    /// `&>` / `&>>` — redirect both stdout and stderr.
    WriteBoth,
    /// `<<<` — here-string.
    HereString,
}

/// Parse a shell command string into a [`ParsedScript`].
///
/// Pure and deterministic: no I/O, no global state. Reproduces shell
/// word-splitting and command segmentation for the gate's subset — quotes,
/// `#` comments, `\`-newline continuation, heredoc bodies, list/pipe operators,
/// redirections, `VAR=` prefixes, path-stripping, reserved-word compounds, and
/// substitution recursion. Never expands globs, variables, or arithmetic.
pub(crate) fn parse(input: &str) -> ParsedScript {
    // Strip heredoc bodies first: their content is literal text, not commands,
    // and their newlines would otherwise segment as list separators. The marker
    // line is kept so the redirection (`<<EOF`) is still seen — and an *unquoted*
    // delimiter's body has its `$(…)` / `` `…` `` command substitutions appended
    // to that marker line, since the shell expands and runs them (bug 46).
    let without_heredocs = strip_heredoc_bodies(input);
    let tokens = lex(&without_heredocs);
    segment(&tokens)
}

// ── Lexer ───────────────────────────────────────────────────────────────────

/// A lexed token: either an operator the shell honors or a word.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A control operator that separates lists / pipelines: `;` `&` `&&` `||`
    /// `|` or a newline.
    Control(Control),
    /// A redirection operator.
    Redir(RedirectOp),
    /// A reserved word in command position (`for`, `do`, `done`, `{`, …).
    Reserved(&'static str),
    /// An opening `(` subshell / `)` close, tracked so segmentation can mark
    /// compounds and skip splitting inside a group.
    Paren(char),
    /// A normal word, with the substitutions found inside it (each the inner
    /// text of a `$(…)` / `` `…` `` / `<(…)` / `>(…)`).
    Word(WordTok),
}

/// A list / pipeline control operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    /// `;` — sequence.
    Semi,
    /// `&` — background / sequence.
    Amp,
    /// `&&` — and-list.
    AndAnd,
    /// `||` — or-list.
    OrOr,
    /// `|` — pipe.
    Pipe,
    /// A newline list separator.
    Newline,
}

/// The five single-byte operators [`lex_operator`] dispatches on. The lexer's
/// main loop already discriminates these bytes before calling, so passing the
/// classified operator in — rather than re-matching `bytes[i]` inside
/// `lex_operator` — makes that dispatch match exhaustive over a finite set, with
/// no impossible-byte `_` arm to harbor an equivalent mutant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpByte {
    /// `;` — sequence.
    Semi,
    /// `&` — background / `&&` and-list / `&>` combined redirect.
    Amp,
    /// `|` — pipe / `||` or-list.
    Pipe,
    /// `>` — the output redirect family.
    Gt,
    /// `<` — the input redirect family.
    Lt,
}

/// A lexed word and the substitutions discovered inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WordTok {
    /// The word text with quotes interpreted away (the value the shell would
    /// pass as an argument), used for name/argv reasoning.
    text: String,
    /// Inner text of each substitution found in the word, in order, for
    /// recursive parsing.
    subs: Vec<String>,
    /// True if any character of the word was quoted — a quoted word can never
    /// be a reserved word or an operator.
    had_quote: bool,
    /// The byte offset in [`text`](Self::text) at which the word's first quoted
    /// run begins, or `None` when the word has no quoted run. Captured as the
    /// length of the accumulated word buffer the moment the first quote opens;
    /// the unquoted leading bytes are pushed 1:1, so this equals the length of
    /// the unquoted prefix in the interpreted `text`. Distinguishes a word whose
    /// `NAME=` is unquoted (an assignment prefix; the value after `=` is data)
    /// from one quoted at or before the `=` (a command literal like `'x=y'`).
    first_quote_at: Option<usize>,
    /// Expansion provenance of the word (which expansion families are live vs
    /// literal), read by the write resolver (ws38 ticket 01).
    meta: WordMeta,
}

/// Note bytes that entered the word through a *literal* channel (quotes /
/// escapes): an expansion-capable metacharacter among them flips
/// [`WordMeta::literal_meta`], marking that the cooked text contains
/// metacharacters that must **not** be expanded.
fn note_literal_bytes(meta: &mut WordMeta, bytes: &[u8]) {
    if bytes
        .iter()
        .any(|b| matches!(b, b'$' | b'`' | b'~' | b'*' | b'?' | b'[' | b'{' | b'}'))
    {
        meta.literal_meta = true;
    }
}

/// Lex the (heredoc-stripped) input into a flat token stream.
///
/// Handles the quote state machine (single / double / `$'…'`), the `'\''`
/// reopen idiom (which falls out naturally from per-quote-run scanning),
/// `\`-newline continuation joining, `#` comment termination, operator
/// recognition, and substitution capture.
#[allow(
    clippy::too_many_lines,
    reason = "one linear character scanner; splitting the quote/operator/word \
              arms into helpers would scatter the shared cursor and word-buffer \
              state and obscure the single pass"
)]
fn lex(input: &str) -> Vec<Token> {
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0;

    // The in-progress word, accumulated as raw bytes across quoted and unquoted
    // runs. UTF-8 conversion is deferred to `flush_word` (a single
    // `from_utf8_lossy` per word), so a multibyte scalar fed one byte at a time
    // is never split across a conversion (bug 43).
    let mut word: Vec<u8> = Vec::new();
    let mut subs: Vec<String> = Vec::new();
    let mut had_quote = false;
    let mut first_quote_at: Option<usize> = None;
    let mut in_word = false;
    let mut meta = WordMeta::default();

    // Flush the in-progress word as a `Word` token (classifying it as a reserved
    // word only when unquoted), resetting the word-building state. Threading the
    // state through `&mut` keeps the reset out of the call sites, so the final
    // flush does not look like a dead store.
    macro_rules! flush_word {
        () => {
            flush_word(
                &mut tokens,
                &mut word,
                &mut subs,
                &mut had_quote,
                &mut first_quote_at,
                &mut in_word,
                &mut meta,
            )
        };
    }

    while i < n {
        let c = bytes[i];
        match c {
            // ── Whitespace (word boundary) ──────────────────────────────────
            b' ' | b'\t' | b'\r' => {
                flush_word!();
                i += 1;
            }
            b'\n' => {
                flush_word!();
                tokens.push(Token::Control(Control::Newline));
                i += 1;
            }
            // ── Comments (only when `#` begins a word) ──────────────────────
            // The shell starts a comment whenever `#` begins a word: at line
            // start, after whitespace/newline (both reset `in_word`), or
            // immediately after an unquoted word-terminating metacharacter. The
            // substitution arms (`<(…)`, `$(…)`) leave `in_word` set but close on
            // `)`, a metacharacter — so `didiff <(sort a)#<(sort)` begins a
            // comment at the `#` (bug 40). A `#` inside quotes / after a backslash
            // / in `$'…'` is consumed by those arms and never reaches here, so it
            // stays literal. The metacharacter check excludes whitespace/newline:
            // they already reset `in_word`, and a `\`-newline join leaves a stray
            // `\n` as the previous byte while still mid-word (`cmd \<nl>#x` glues
            // `#x` onto `cmd`).
            b'#' if !in_word || (i > 0 && is_comment_boundary_metachar(bytes[i - 1])) => {
                // Run to end of line; the line separator itself is emitted by
                // the `\n` arm on the next iteration.
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            // ── Backslash ───────────────────────────────────────────────────
            b'\\' => {
                if i + 1 < n && bytes[i + 1] == b'\n' {
                    // `\`-newline line continuation: drop both, joining lines
                    // (bug 30).
                    i += 2;
                } else if i + 2 < n && bytes[i + 1] == b'\r' && bytes[i + 2] == b'\n' {
                    // CRLF continuation.
                    i += 3;
                } else if i + 1 < n {
                    // Escaped char: keep the following byte literally.
                    in_word = true;
                    note_literal_bytes(&mut meta, &bytes[i + 1..=i + 1]);
                    push_byte(&mut word, bytes[i + 1]);
                    i += 2;
                } else {
                    // Trailing backslash at end of input — keep literally.
                    in_word = true;
                    word.push(b'\\');
                    i += 1;
                }
            }
            // ── Single quotes ───────────────────────────────────────────────
            b'\'' => {
                in_word = true;
                had_quote = true;
                if first_quote_at.is_none() {
                    first_quote_at = Some(word.len());
                }
                let end = memchr_byte(bytes, b'\'', i + 1).unwrap_or(n);
                note_literal_bytes(&mut meta, &bytes[i + 1..end.min(n)]);
                push_bytes(&mut word, &bytes[i + 1..end.min(n)]);
                i = if end < n { end + 1 } else { n };
            }
            // ── Double quotes ───────────────────────────────────────────────
            b'"' => {
                in_word = true;
                had_quote = true;
                if first_quote_at.is_none() {
                    first_quote_at = Some(word.len());
                }
                i = lex_double_quote(bytes, i + 1, &mut word, &mut subs, &mut meta);
            }
            // ── `$` — `$'…'`, `$(…)`, or a plain `$word` ────────────────────
            b'$' => {
                in_word = true;
                if i + 1 < n && bytes[i + 1] == b'\'' {
                    // `$'…'` ANSI-C quoting.
                    had_quote = true;
                    if first_quote_at.is_none() {
                        first_quote_at = Some(word.len());
                    }
                    let mark = word.len();
                    i = lex_ansi_c_quote(bytes, i + 2, &mut word);
                    note_literal_bytes(&mut meta, &word[mark..]);
                } else if i + 1 < n && bytes[i + 1] == b'(' {
                    // `$(…)` command substitution (or `$((…))` arithmetic).
                    let (inner, next) = scan_balanced(bytes, i + 2, b'(', b')');
                    subs.push(inner);
                    meta.value_subs = true;
                    i = next;
                } else {
                    meta.live_dollar = true;
                    word.push(b'$');
                    i += 1;
                }
            }
            // ── Backtick command substitution ───────────────────────────────
            b'`' => {
                in_word = true;
                let (inner, next) = scan_backtick(bytes, i + 1);
                subs.push(inner);
                meta.value_subs = true;
                i = next;
            }
            // ── Process substitution `<(…)` / `>(…)` ────────────────────────
            b'<' | b'>' if i + 1 < n && bytes[i + 1] == b'(' && !is_redir_fd_context(bytes, i) => {
                in_word = true;
                let (inner, next) = scan_balanced(bytes, i + 2, b'(', b')');
                subs.push(inner);
                meta.process_subs = true;
                i = next;
            }
            // ── Parentheses (subshell grouping) ─────────────────────────────
            b'(' | b')' if !in_word => {
                flush_word!();
                tokens.push(Token::Paren(c as char));
                i += 1;
            }
            // ── fd-numbered redirect at a word boundary (`2>`, `1>&2`) ──────
            // A digit run that abuts a `<`/`>` is the source fd of a redirect,
            // not an argument word, so it must not leak into argv.
            b'0'..=b'9' if !in_word && fd_redirect_after(bytes, i) => {
                let op_at = i + digit_run_len(bytes, i);
                // `fd_redirect_after` guarantees `bytes[op_at]` is `<` or `>`, so
                // the catch-all is reached exactly for `<` (an fd read redirect).
                let op = match bytes[op_at] {
                    b'>' => OpByte::Gt,
                    _ => OpByte::Lt,
                };
                i = lex_operator(bytes, op_at, op, &mut tokens);
            }
            // ── Operators (only at a word boundary) ─────────────────────────
            b';' | b'&' | b'|' | b'<' | b'>' => {
                flush_word!();
                // This arm admits only the five operator bytes; matching four
                // explicitly leaves the catch-all reachable exactly for `<`.
                let op = match c {
                    b';' => OpByte::Semi,
                    b'&' => OpByte::Amp,
                    b'|' => OpByte::Pipe,
                    b'>' => OpByte::Gt,
                    _ => OpByte::Lt,
                };
                i = lex_operator(bytes, i, op, &mut tokens);
            }
            // ── Ordinary word byte ──────────────────────────────────────────
            _ => {
                // An unquoted metacharacter is a *live* expansion candidate:
                // globs and braces anywhere in the word, tilde only when it
                // opens the word (`in_word` false and nothing accumulated —
                // `''~` is quoted-opened and does not tilde-expand).
                match c {
                    b'*' | b'?' | b'[' => meta.live_glob = true,
                    b'{' | b'}' => meta.live_brace = true,
                    b'~' if !in_word && word.is_empty() => meta.live_tilde = true,
                    _ => {}
                }
                in_word = true;
                push_byte(&mut word, c);
                i += 1;
            }
        }
    }
    flush_word!();
    tokens
}

/// Flush the in-progress word into `tokens` (when one is pending), then reset
/// the word-building state. Factored out of [`lex`] so the per-call reset lives
/// behind a function boundary — the final flush at end of input then does not
/// read as a dead store.
#[allow(
    clippy::too_many_arguments,
    reason = "the lexer's word-building state is threaded as one flat set of \
              &mut locals; bundling them into a struct would obscure the single \
              linear scan in `lex`"
)]
fn flush_word(
    tokens: &mut Vec<Token>,
    word: &mut Vec<u8>,
    subs: &mut Vec<String>,
    had_quote: &mut bool,
    first_quote_at: &mut Option<usize>,
    in_word: &mut bool,
    meta: &mut WordMeta,
) {
    if *in_word {
        // Convert the accumulated raw bytes to text exactly once, at the word
        // boundary — a complete buffer, so a multibyte scalar is never split
        // across the `from_utf8_lossy` (bug 43).
        let text = String::from_utf8_lossy(&std::mem::take(word)).into_owned();
        tokens.push(classify_word(WordTok {
            text,
            subs: std::mem::take(subs),
            had_quote: *had_quote,
            first_quote_at: *first_quote_at,
            meta: std::mem::take(meta),
        }));
        *had_quote = false;
        *first_quote_at = None;
        *in_word = false;
    }
}

/// Whether `byte` is an unquoted word-terminating metacharacter after which a
/// `#` begins a comment (`)` `(` `;` `&` `|` `<` `>`).
///
/// Whitespace and newline are *excluded*: those already flush the word and
/// reset `in_word`, so the `!in_word` gate covers them — and including the
/// newline here would mis-fire on a `\`-newline join, whose stray `\n` precedes
/// a `#` that is still mid-word (`cmd \<nl>#x` is one word, not a comment). In
/// practice only a substitution-closing `)` reaches this check with `in_word`
/// still set, but matching the full metacharacter set documents the shell rule
/// (decision 020 §3, bug 40).
const fn is_comment_boundary_metachar(byte: u8) -> bool {
    matches!(byte, b')' | b'(' | b';' | b'&' | b'|' | b'<' | b'>')
}

/// Whether the `<`/`>` at index `i` is an fd-redirection context rather than a
/// process substitution. A leading digit run abutting the operator (`2>(…)`)
/// means a redirect; a bare `<(`/`>(` is process substitution.
fn is_redir_fd_context(bytes: &[u8], i: usize) -> bool {
    let mut j = i;
    while j > 0 && bytes[j - 1].is_ascii_digit() {
        j -= 1;
    }
    j < i
}

/// The length of the ASCII-digit run starting at `i`.
fn digit_run_len(bytes: &[u8], i: usize) -> usize {
    let mut j = i;
    while j < bytes.len() && bytes[j].is_ascii_digit() {
        j += 1;
    }
    j - i
}

/// Whether the digit run starting at `i` is the source fd of a redirection —
/// i.e. it is immediately followed by `<` or `>` (`2>file`, `1>&2`). Such a run
/// belongs to the redirect operator, not to a word.
fn fd_redirect_after(bytes: &[u8], i: usize) -> bool {
    let end = i + digit_run_len(bytes, i);
    matches!(bytes.get(end), Some(b'<' | b'>'))
}

/// Lex a double-quoted run starting just after the opening `"`, appending the
/// interpreted content to `word` and any `$(…)` / `` `…` `` substitutions found
/// inside to `subs`. Returns the index just past the closing `"` (or end).
///
/// Provenance: inside double quotes `$` still expands (live), while globs,
/// braces, and tildes are quoted (literal) — `meta` records both channels for
/// the write resolver.
fn lex_double_quote(
    bytes: &[u8],
    start: usize,
    word: &mut Vec<u8>,
    subs: &mut Vec<String>,
    meta: &mut WordMeta,
) -> usize {
    let n = bytes.len();
    let mut i = start;
    while i < n {
        match bytes[i] {
            b'"' => return i + 1,
            b'\\' => {
                // Inside double quotes, backslash escapes only `$`, `` ` ``,
                // `"`, `\`, and newline; otherwise it stays literal.
                if i + 1 < n {
                    let next = bytes[i + 1];
                    if next == b'\n' {
                        i += 2; // line continuation inside double quotes
                    } else if matches!(next, b'$' | b'`' | b'"' | b'\\') {
                        note_literal_bytes(meta, &bytes[i + 1..=i + 1]);
                        push_byte(word, next);
                        i += 2;
                    } else {
                        word.push(b'\\');
                        i += 1;
                    }
                } else {
                    word.push(b'\\');
                    i += 1;
                }
            }
            b'`' => {
                let (inner, next) = scan_backtick(bytes, i + 1);
                subs.push(inner);
                meta.value_subs = true;
                i = next;
            }
            b'$' if i + 1 < n && bytes[i + 1] == b'(' => {
                let (inner, next) = scan_balanced(bytes, i + 2, b'(', b')');
                subs.push(inner);
                meta.value_subs = true;
                i = next;
            }
            other => {
                if other == b'$' {
                    // `"$NAME"` expands — a live dollar even though quoted.
                    meta.live_dollar = true;
                } else {
                    note_literal_bytes(meta, &bytes[i..=i]);
                }
                push_byte(word, other);
                i += 1;
            }
        }
    }
    n
}

/// Lex a `$'…'` ANSI-C quoted run starting just after the opening `'`,
/// appending interpreted content to `word`. Returns the index past the closing
/// `'`. A backslash escapes the next character (so `\'` stays inside the run).
fn lex_ansi_c_quote(bytes: &[u8], start: usize, word: &mut Vec<u8>) -> usize {
    let n = bytes.len();
    let mut i = start;
    while i < n {
        match bytes[i] {
            b'\'' => return i + 1,
            b'\\' if i + 1 < n => {
                // Best-effort: keep the escaped byte literally. The gate never
                // needs the exact C-escape value, only that the run is a quoted
                // argument that cannot reach command/operator position.
                push_byte(word, bytes[i + 1]);
                i += 2;
            }
            other => {
                push_byte(word, other);
                i += 1;
            }
        }
    }
    n
}

/// Lex the operator `op` beginning at index `i`, pushing the corresponding token
/// (looking ahead for the multi-byte forms `&&`, `&>`, `>>`, `<<<`, `<&`, …).
/// Returns the index just past the operator. `op` is the classified leading byte
/// `bytes[i]`, threaded in by the caller so this dispatch stays exhaustive over a
/// finite set — there is no impossible-byte `_` arm to leave dead.
fn lex_operator(bytes: &[u8], i: usize, op: OpByte, tokens: &mut Vec<Token>) -> usize {
    let n = bytes.len();
    match op {
        OpByte::Semi => {
            tokens.push(Token::Control(Control::Semi));
            i + 1
        }
        OpByte::Amp => {
            if i + 1 < n && bytes[i + 1] == b'&' {
                tokens.push(Token::Control(Control::AndAnd));
                i + 2
            } else if i + 1 < n && bytes[i + 1] == b'>' {
                // `&>` / `&>>` redirect both stdout and stderr.
                tokens.push(Token::Redir(RedirectOp::WriteBoth));
                if i + 2 < n && bytes[i + 2] == b'>' {
                    i + 3
                } else {
                    i + 2
                }
            } else {
                tokens.push(Token::Control(Control::Amp));
                i + 1
            }
        }
        OpByte::Pipe => {
            if i + 1 < n && bytes[i + 1] == b'|' {
                tokens.push(Token::Control(Control::OrOr));
                i + 2
            } else {
                tokens.push(Token::Control(Control::Pipe));
                i + 1
            }
        }
        OpByte::Gt => {
            if i + 1 < n && bytes[i + 1] == b'>' {
                tokens.push(Token::Redir(RedirectOp::Append));
                i + 2
            } else if i + 1 < n && bytes[i + 1] == b'&' {
                tokens.push(Token::Redir(RedirectOp::DupOut));
                i + 2
            } else if i + 1 < n && bytes[i + 1] == b'|' {
                // `>|` clobber — treat as a plain write.
                tokens.push(Token::Redir(RedirectOp::Write));
                i + 2
            } else {
                tokens.push(Token::Redir(RedirectOp::Write));
                i + 1
            }
        }
        OpByte::Lt => {
            if i + 2 < n && bytes[i + 1] == b'<' && bytes[i + 2] == b'<' {
                tokens.push(Token::Redir(RedirectOp::HereString));
                i + 3
            } else if i + 1 < n && bytes[i + 1] == b'<' {
                // `<<` heredoc marker — the body was stripped; consume the
                // operator and let the following word be its delimiter.
                tokens.push(Token::Redir(RedirectOp::Read));
                i + 2
            } else if i + 1 < n && bytes[i + 1] == b'&' {
                tokens.push(Token::Redir(RedirectOp::DupIn));
                i + 2
            } else {
                tokens.push(Token::Redir(RedirectOp::Read));
                i + 1
            }
        }
    }
}

/// Classify a finished word as a reserved word (only when unquoted) or a plain
/// word token.
fn classify_word(w: WordTok) -> Token {
    if !w.had_quote
        && let Some(reserved) = reserved_word(&w.text)
    {
        return Token::Reserved(reserved);
    }
    Token::Word(w)
}

/// The reserved words that introduce or delimit compound commands. Returns the
/// canonical static string when `text` is one of them.
fn reserved_word(text: &str) -> Option<&'static str> {
    const WORDS: [&str; 15] = [
        "for", "select", "while", "until", "if", "case", "do", "done", "then", "elif", "else",
        "fi", "esac", "in", "{",
    ];
    WORDS.iter().copied().find(|&w| w == text)
}

// ── Heredoc stripping ─────────────────────────────────────────────────────────

/// The closing delimiter the body-strip is scanning for, plus whether the
/// heredoc allows the terminator to be indented (`<<-`).
struct HeredocClose {
    /// The delimiter word (`EOF`), with surrounding quotes already removed by
    /// the marker capture.
    marker: String,
    /// `<<-EOF` — the terminator (and body lines) may carry leading tabs, which
    /// the shell strips. A plain `<<EOF` terminator must sit at column 0.
    dash: bool,
}

impl HeredocClose {
    /// Whether `line` is this heredoc's closing-delimiter line.
    ///
    /// For a plain `<<EOF`, only the bare delimiter at column 0 closes it — an
    /// *indented* line that happens to read `EOF` is body text, not the
    /// terminator (the historical leak: `trim()`-comparing closed early and let
    /// the rest of the body reach the gate). For `<<-EOF`, the shell strips
    /// leading tabs from the terminator, so a tab-indented delimiter still
    /// closes.
    fn closes(&self, line: &str) -> bool {
        let candidate = if self.dash {
            line.trim_start_matches('\t')
        } else {
            line
        };
        candidate == self.marker
    }
}

/// The heredoc currently being skipped: how its terminator is matched, whether
/// its body undergoes expansion (an unquoted delimiter), where its marker line
/// sits in the stripped output (so an expanded body's substitutions can be
/// appended to it), and the accumulated body text.
struct ActiveHeredoc {
    /// The terminator matcher (delimiter + `<<-` indentation rule).
    close: HeredocClose,
    /// Whether the shell expands this body. `true` only for a wholly
    /// unquoted/unescaped delimiter (`<<EOF`); any quote (`<<'EOF'` / `<<"EOF"`)
    /// or backslash (`<<\EOF`) makes the body literal stdin (bug 46).
    expand: bool,
    /// Index of the marker line (`cat <<EOF`) in the stripped output, so an
    /// expanded body's command substitutions can be appended to it.
    marker_idx: usize,
    /// The accumulated body text (only collected when `expand`).
    body: String,
}

/// Remove heredoc bodies *and* their closing-delimiter lines, keeping the
/// marker line (e.g. `cat <<EOF`) intact so the `<<` redirection is still
/// lexed.
///
/// A *quoted*-delimiter heredoc body (`<<'EOF'` / `<<"EOF"` / `<<\EOF`) is
/// literal stdin, never commands — stripping it before the lexer is what keeps
/// body prose (a `catenary diagnostics` named in a commit message, a `;`/`&&`
/// in a sentence) out of every gate. An *unquoted* `<<EOF` body is **not**
/// opaque: the shell expands `$(…)` / `` `…` `` in it and runs them, so those
/// substitution spans are appended to the marker line — projected as command
/// positions on the heredoc-owning command — while the inert literal text is
/// still dropped (bug 46). The terminator match is shell-faithful so a
/// delimiter-like word *inside* the body does not close the heredoc early: a
/// plain `<<EOF` closes only on a bare `EOF` at column 0, while `<<-EOF` permits
/// a tab-indented one. The quoted (`<<'EOF'`) and indented (`<<-EOF`) marker
/// forms are recognized by
/// [`HEREDOC_MARKER_RE`](super::patterns::HEREDOC_MARKER_RE).
fn strip_heredoc_bodies(input: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut active: Option<ActiveHeredoc> = None;
    for line in input.split('\n') {
        // `closed` flags the terminator line, deferring the `active.take()`
        // until after the `as_mut()` borrow ends (so no `expect` is needed).
        let closed = match active.as_mut() {
            // Body line of the active heredoc (terminator not yet reached):
            // dropped from the lexed stream, but accumulated when the body
            // expands so its substitutions can be projected.
            Some(h) if !h.close.closes(line) => {
                if h.expand {
                    h.body.push_str(line);
                    h.body.push('\n');
                }
                false
            }
            // The closing-delimiter line.
            Some(_) => true,
            None => {
                out.push(line.to_string());
                if let Some(caps) = HEREDOC_MARKER_RE.captures(line)
                    && let Some(m) = caps.get(1)
                {
                    let whole = caps.get(0).map_or("", |w| w.as_str());
                    active = Some(ActiveHeredoc {
                        close: HeredocClose {
                            marker: m.as_str().to_string(),
                            // A `<<-` marker has the dash immediately after `<<`.
                            dash: whole.starts_with("<<-"),
                        },
                        // Any quote or backslash in the delimiter run defuses
                        // expansion; a bare `<<EOF` expands and runs its body
                        // substitutions (bug 46).
                        expand: !whole.bytes().any(|b| matches!(b, b'\'' | b'"' | b'\\')),
                        marker_idx: out.len() - 1,
                        body: String::new(),
                    });
                }
                false
            }
        };
        if closed && let Some(h) = active.take() {
            flush_heredoc_subs(&mut out, &h);
        }
    }
    // An unterminated heredoc still flushes whatever it accumulated — the safe
    // (over-count) direction; the malformed input is a fail-closed deny anyway.
    if let Some(h) = active.take() {
        flush_heredoc_subs(&mut out, &h);
    }
    out.join("\n")
}

/// Finalize a closed (or end-of-input) heredoc: when its delimiter was unquoted,
/// append the body's command substitutions to its marker line so they lex as
/// substitutions on the heredoc-owning command (bug 46). A quoted/inert body, or
/// one with no substitution, leaves the marker line untouched.
fn flush_heredoc_subs(out: &mut [String], heredoc: &ActiveHeredoc) {
    if !heredoc.expand {
        return;
    }
    let subs = heredoc_command_subs(&heredoc.body);
    if subs.is_empty() {
        return;
    }
    let marker = &mut out[heredoc.marker_idx];
    marker.push(' ');
    marker.push_str(&subs);
}

/// Extract an unquoted heredoc body's command-substitution spans (`$(…)` and
/// `` `…` ``), in document order, space-joined — the spans the shell expands and
/// executes when the delimiter is unquoted (bug 46).
///
/// The literal body text is inert stdin and dropped; only the substitution spans
/// survive, to be re-lexed (the caller appends them to the marker line) as
/// substitutions on the heredoc-owning command. The scan honours the heredoc
/// body's escape rule — a backslash defuses a following `$` / `` ` `` / `\` — and
/// is *not* quote-aware at the body level (single/double quotes are literal in a
/// heredoc body and never guard a substitution). `$((…))` arithmetic runs no
/// command and is skipped. The captured span's *inner* parens/quotes are still
/// balanced quote-aware by [`scan_balanced`] / [`scan_backtick`], so a `)` or
/// backtick inside a nested string does not end the span early.
fn heredoc_command_subs(body: &str) -> String {
    let bytes = body.as_bytes();
    let n = bytes.len();
    let mut spans: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < n {
        match bytes[i] {
            // A backslash defuses the next byte (the body's only escape).
            b'\\' if i + 1 < n => i += 2,
            // `$(…)` command substitution — capture; `$((…))` arithmetic — skip.
            b'$' if i + 1 < n && bytes[i + 1] == b'(' => {
                let arithmetic = i + 2 < n && bytes[i + 2] == b'(';
                let (_, next) = scan_balanced(bytes, i + 2, b'(', b')');
                if !arithmetic {
                    spans.push(&body[i..next]);
                }
                i = next;
            }
            // `` `…` `` backtick command substitution — capture.
            b'`' => {
                let (_, next) = scan_backtick(bytes, i + 1);
                spans.push(&body[i..next]);
                i = next;
            }
            _ => i += 1,
        }
    }
    spans.join(" ")
}

// ── Segmentation ──────────────────────────────────────────────────────────────

/// Segment a token stream into a [`ParsedScript`]: split on list operators into
/// pipelines, split each pipeline on `|` into simple commands, and build each
/// command (name, argv, redirects, substitutions, compound flag). The list
/// operator that *terminates* each pipeline is otherwise discarded, except a
/// bare `&` sets [`Pipeline::backgrounded`].
fn segment(tokens: &[Token]) -> ParsedScript {
    let mut pipelines = Vec::new();
    for (pipe_tokens, sep) in split_on_list_ops(tokens) {
        if pipe_tokens.is_empty() {
            continue;
        }
        let mut pipeline = build_pipeline(pipe_tokens);
        if pipeline.commands.is_empty() {
            continue;
        }
        pipeline.backgrounded = sep == Some(Control::Amp);
        pipeline.terminator = sep.map(|c| match c {
            Control::AndAnd => ListOp::And,
            Control::OrOr => ListOp::Or,
            // `|` never terminates a list segment (stripped by the pipeline
            // split); folded into the unconditional arm for totality.
            Control::Semi | Control::Amp | Control::Newline | Control::Pipe => ListOp::Seq,
        });
        pipelines.push(pipeline);
    }
    ParsedScript { pipelines }
}

/// Split a token slice on the list operators `;` `&` `&&` `||` newline,
/// returning each segment paired with the [`Control`] operator that terminated
/// it (`None` for the final, unterminated segment). The operator is needed so a
/// bare `&` can mark the preceding pipeline backgrounded.
fn split_on_list_ops(tokens: &[Token]) -> Vec<(&[Token], Option<Control>)> {
    let mut parts = Vec::new();
    let mut last = 0;
    for (i, t) in tokens.iter().enumerate() {
        if let Token::Control(
            c @ (Control::Semi | Control::Amp | Control::AndAnd | Control::OrOr | Control::Newline),
        ) = t
        {
            parts.push((&tokens[last..i], Some(*c)));
            last = i + 1;
        }
    }
    parts.push((&tokens[last..], None));
    parts
}

/// Build a pipeline from a slice of tokens with no top-level list operators.
fn build_pipeline(tokens: &[Token]) -> Pipeline {
    let mut commands = Vec::new();
    for stage in split_on(tokens, |t| matches!(t, Token::Control(Control::Pipe))) {
        if let Some(cmd) = build_command(stage) {
            commands.push(cmd);
        }
    }
    Pipeline {
        commands,
        backgrounded: false,
        terminator: None,
    }
}

/// Build a single simple command from a slice of word / redirect / reserved /
/// paren tokens (no control operators). Returns `None` for an empty stage.
fn build_command(tokens: &[Token]) -> Option<SimpleCommand> {
    if tokens.is_empty() {
        return None;
    }

    let mut cmd = SimpleCommand::default();
    let mut words: Vec<&WordTok> = Vec::new();

    let mut idx = 0;
    while idx < tokens.len() {
        match &tokens[idx] {
            // `for VAR in LIST` / `select VAR in LIST`: the loop variable and the
            // iteration list are *not* command positions (only the body, after
            // `do`, is — and `;`/newline already split it into its own segment).
            // Sweep the words for their substitutions (`for f in $(ls)` still
            // runs `ls` during expansion) but assign no command name, so the
            // loop variable never reaches the allowlist (ticket 04: a `for` loop
            // of allowlisted commands must be allowed).
            Token::Reserved("for" | "select") => {
                cmd.is_compound = true;
                // Capture the loop variable so the write resolver can taint it
                // (its runtime value is per-iteration, never resolvable).
                if let Some(var) = tokens.get(idx + 1).and_then(token_word) {
                    cmd.loop_var = Some(var.text.clone());
                }
                collect_subs_only(&tokens[idx..], &mut cmd);
                idx = tokens.len();
            }
            Token::Reserved(_) | Token::Paren('(') => {
                // Any other reserved word or `(` in command position marks a
                // compound; sweep the rest of the segment's words so inner
                // command positions (a `while`/`if` condition, a `do`/`{`/`(`
                // body) and substitutions are still discovered.
                cmd.is_compound = true;
                collect_rest(&tokens[idx..], &mut words, &mut cmd);
                idx = tokens.len();
            }
            // A close `)` (the opening `(` is handled above) carries no command
            // position. A `Token::Control` cannot actually reach a built stage —
            // `split_on_list_ops` / `build_pipeline` strip every list/pipe operator
            // before `build_command` runs — so it is folded in here rather than
            // given its own arm, keeping the match exhaustive without a separate
            // dead branch to skip.
            Token::Paren(_) | Token::Control(_) => {
                idx += 1;
            }
            Token::Redir(op) => {
                // The next word (if any) is the redirect target.
                if let Some(t) = tokens.get(idx + 1).and_then(token_word) {
                    cmd.redirects.push(Redirect {
                        op: *op,
                        target: t.text.clone(),
                        target_meta: t.meta,
                    });
                    collect_subs(t, &mut cmd);
                    idx += 2;
                } else {
                    cmd.redirects.push(Redirect {
                        op: *op,
                        target: String::new(),
                        target_meta: WordMeta::default(),
                    });
                    idx += 1;
                }
            }
            Token::Word(w) => {
                words.push(w);
                collect_subs(w, &mut cmd);
                idx += 1;
            }
        }
    }

    assign_name_and_argv(&words, &mut cmd);

    if cmd.name.is_none()
        && cmd.argv.is_empty()
        && cmd.redirects.is_empty()
        && cmd.substitutions.is_empty()
        && cmd.assignments.is_empty()
        && !cmd.is_compound
    {
        return None;
    }
    Some(cmd)
}

/// After hitting a reserved word / `(`, sweep the remaining tokens for words
/// (to find inner command positions), substitutions, and redirects — ignoring
/// only further reserved/paren/control *structure*. Gate 04 owns compound-body
/// policy; the parser surfaces the inner words, recursed substitutions, and the
/// redirects.
///
/// Redirects are collected because a redirect *following* the compound's close
/// binds to the whole compound and must reach the gate's `allow_file_redirects`
/// enforcement (bug 45). When a compound has no top-level list operator its
/// closing delimiter and any trailing redirect stay in the same segment that
/// began with the `(` / reserved word, so they route through here rather than
/// the `Token::Redir` arm of [`build_command`]. The brace-group form escaped the
/// bug only by accident — its `}` is not a reserved word, so the `;` before it
/// splits the trailing-redirect tail into its own simple command. The subshell
/// `( … )` arm had no such split and so dropped the redirect. Sweeping every
/// `Token::Redir` here (its following word consumed as the target, mirroring
/// [`build_command`]) attaches the redirect at every nesting level — matching the
/// flat redirect multiset the brush reference parser projects (`( ( a ) > x ) > y`
/// → two output redirects).
fn collect_rest<'a>(tokens: &'a [Token], words: &mut Vec<&'a WordTok>, cmd: &mut SimpleCommand) {
    let mut idx = 0;
    while idx < tokens.len() {
        match &tokens[idx] {
            Token::Word(w) => {
                words.push(w);
                collect_subs(w, cmd);
                idx += 1;
            }
            Token::Redir(op) => {
                // The next word (if any) is the redirect target — consume it so a
                // bound redirect on the compound (`( cmd ) > out`) reaches the
                // gate and its target word does not leak into the swept words.
                if let Some(t) = tokens.get(idx + 1).and_then(token_word) {
                    cmd.redirects.push(Redirect {
                        op: *op,
                        target: t.text.clone(),
                        target_meta: t.meta,
                    });
                    collect_subs(t, cmd);
                    idx += 2;
                } else {
                    cmd.redirects.push(Redirect {
                        op: *op,
                        target: String::new(),
                        target_meta: WordMeta::default(),
                    });
                    idx += 1;
                }
            }
            Token::Reserved(_) | Token::Paren(_) | Token::Control(_) => {
                idx += 1;
            }
        }
    }
}

/// Sweep the remaining tokens of a `for`/`select` segment for *substitutions
/// only* (`for f in $(ls)` still runs `ls` during list expansion), assigning no
/// command name — the loop variable and the iteration words are not command
/// positions.
fn collect_subs_only(tokens: &[Token], cmd: &mut SimpleCommand) {
    for tok in tokens {
        if let Token::Word(w) = tok {
            collect_subs(w, cmd);
        }
    }
}

/// Recurse into a word's substitutions, parsing each as its own script.
fn collect_subs(w: &WordTok, cmd: &mut SimpleCommand) {
    for sub in &w.subs {
        cmd.substitutions.push(parse(sub));
    }
}

/// Whether `w` is a leading `NAME=value` assignment prefix. The `NAME=` part
/// must be unquoted — the value after `=` may be quoted and is data. A word
/// whose first quoted run opens at or before the assignment `=` (e.g. `'x=y'`)
/// is a command literal, not an assignment prefix.
fn is_assignment_prefix(w: &WordTok) -> bool {
    ENV_VAR_RE.is_match(&w.text)
        && w.first_quote_at
            .is_none_or(|q| w.text.find('=').is_some_and(|eq| eq < q))
}

/// Assign the command name (after `VAR=` prefixes, path-stripped) and argv from
/// the collected words. The skipped assignment words are retained on
/// [`SimpleCommand::assignments`] for the write resolver, and each argv word
/// carries its [`WordMeta`] in the index-parallel `argv_meta`.
fn assign_name_and_argv(words: &[&WordTok], cmd: &mut SimpleCommand) {
    // Skip leading `VAR=value` assignment words to find the command position.
    let mut name_idx = None;
    for (i, w) in words.iter().enumerate() {
        // An assignment prefix is a `NAME=...` whose `NAME=` is unquoted; the
        // value after `=` may be quoted and is data, not a command.
        if is_assignment_prefix(w) {
            if let Some((name, value)) = w.text.split_once('=') {
                cmd.assignments.push(Assignment {
                    name: name.to_string(),
                    value: value.to_string(),
                    meta: w.meta,
                });
            }
            continue;
        }
        name_idx = Some(i);
        break;
    }

    let Some(name_idx) = name_idx else {
        // All words were assignments (or there were none) — no command word.
        return;
    };

    cmd.name = Some(path_strip(&words[name_idx].text));
    cmd.argv = words[name_idx + 1..]
        .iter()
        .map(|w| w.text.clone())
        .collect();
    cmd.argv_meta = words[name_idx + 1..].iter().map(|w| w.meta).collect();
}

/// Strip the leading path from a command word (`/usr/bin/git` → `git`).
fn path_strip(raw: &str) -> String {
    Path::new(raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(raw)
        .to_string()
}

// ── Token / byte helpers ──────────────────────────────────────────────────────

/// The word inside a [`Token::Word`], or `None` for any other token.
const fn token_word(tok: &Token) -> Option<&WordTok> {
    match tok {
        Token::Word(w) => Some(w),
        Token::Control(_) | Token::Redir(_) | Token::Reserved(_) | Token::Paren(_) => None,
    }
}

/// Split a token slice into sub-slices on every token matching `is_sep`,
/// dropping the separators. Empty leading/trailing/adjacent segments are
/// preserved (callers skip empties).
fn split_on(tokens: &[Token], is_sep: impl Fn(&Token) -> bool) -> Vec<&[Token]> {
    let mut parts = Vec::new();
    let mut last = 0;
    for (i, t) in tokens.iter().enumerate() {
        if is_sep(t) {
            parts.push(&tokens[last..i]);
            last = i + 1;
        }
    }
    parts.push(&tokens[last..]);
    parts
}

/// Find the next occurrence of `needle` in `bytes` at or after `from`.
fn memchr_byte(bytes: &[u8], needle: u8, from: usize) -> Option<usize> {
    (from..bytes.len()).find(|&j| bytes[j] == needle)
}

/// Append a single byte (from a valid `&str`, scanned in order) to the word
/// accumulator.
///
/// The accumulator holds raw bytes; UTF-8 validation is deferred to
/// [`flush_word`], which converts the complete buffer exactly once. Appending
/// byte-by-byte here is therefore always correct, even for a multibyte scalar
/// fed one byte at a time — the bytes are never split across a conversion.
fn push_byte(s: &mut Vec<u8>, b: u8) {
    s.push(b);
}

/// Append a byte slice (a sub-run of a valid `&str`) to the word accumulator.
fn push_bytes(s: &mut Vec<u8>, bytes: &[u8]) {
    s.extend_from_slice(bytes);
}

/// Scan a balanced `open`/`close` run starting at `start` (just past the first
/// `open`), returning the inner text (excluding the outermost delimiters) and
/// the index just past the matching `close`. Quote- and nesting-aware so a
/// close delimiter inside a string or a nested group does not end the run.
fn scan_balanced(bytes: &[u8], start: usize, open: u8, close: u8) -> (String, usize) {
    let n = bytes.len();
    let mut depth = 1u32;
    let mut i = start;
    while i < n {
        match bytes[i] {
            b'\'' => {
                i = memchr_byte(bytes, b'\'', i + 1).map_or(n, |e| e + 1);
            }
            b'"' => {
                i = skip_double_quote(bytes, i + 1);
            }
            b'\\' if i + 1 < n => i += 2,
            c if c == open => {
                depth += 1;
                i += 1;
            }
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    let inner = String::from_utf8_lossy(&bytes[start..i]).into_owned();
                    return (inner, i + 1);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    (String::from_utf8_lossy(&bytes[start..n]).into_owned(), n)
}

/// Scan a backtick command substitution starting at `start` (just past the
/// opening `` ` ``), returning the inner text and the index just past the
/// closing backtick. Backslash escapes the closing backtick.
fn scan_backtick(bytes: &[u8], start: usize) -> (String, usize) {
    let n = bytes.len();
    let mut i = start;
    while i < n {
        match bytes[i] {
            b'\\' if i + 1 < n => i += 2,
            b'`' => {
                let inner = String::from_utf8_lossy(&bytes[start..i]).into_owned();
                return (inner, i + 1);
            }
            _ => i += 1,
        }
    }
    (String::from_utf8_lossy(&bytes[start..n]).into_owned(), n)
}

/// Skip past a double-quoted run starting at `start` (just past the opening
/// `"`), returning the index past the closing `"`. Used inside `scan_balanced`
/// where the inner content is captured verbatim, not interpreted.
fn skip_double_quote(bytes: &[u8], start: usize) -> usize {
    let n = bytes.len();
    let mut i = start;
    while i < n {
        match bytes[i] {
            b'\\' if i + 1 < n => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    n
}

// ── Projection helpers (the gate / oracle view) ──────────────────────────────

impl ParsedScript {
    /// The ordered list of command-position names across every pipeline and
    /// every recursed substitution. This is the projection the allowlist gate
    /// (ticket 02) and the differential oracle (ticket 05) read.
    pub(crate) fn command_positions(&self) -> Vec<String> {
        let mut names = Vec::new();
        self.collect_command_positions(&mut names);
        names
    }

    /// Recursive worker for [`Self::command_positions`].
    fn collect_command_positions(&self, names: &mut Vec<String>) {
        for pipeline in &self.pipelines {
            for cmd in &pipeline.commands {
                if let Some(name) = &cmd.name {
                    names.push(name.clone());
                }
                for sub in &cmd.substitutions {
                    sub.collect_command_positions(names);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect/assert-style helpers for readable failures"
)]
mod tests {
    use super::{ParsedScript, RedirectOp, SimpleCommand, parse};

    /// The flat command-position projection, the gate's primary view.
    fn positions(input: &str) -> Vec<String> {
        parse(input).command_positions()
    }

    /// The single top-level command of a one-pipeline, one-command script.
    fn sole(input: &str) -> SimpleCommand {
        let script = parse(input);
        assert_eq!(
            script.pipelines.len(),
            1,
            "expected exactly one pipeline for {input:?}, got {script:#?}"
        );
        let pipeline = &script.pipelines[0];
        assert_eq!(
            pipeline.commands.len(),
            1,
            "expected exactly one command for {input:?}, got {pipeline:#?}"
        );
        pipeline.commands[0].clone()
    }

    // ── Ticket-listed assert-on-value cases ──────────────────────────────────

    #[test]
    fn comment_dropped() {
        // `make test  # run it` → one command `make`, comment dropped.
        let cmd = sole("make test  # run it");
        assert_eq!(cmd.name.as_deref(), Some("make"));
        assert_eq!(cmd.argv, vec!["test"]);
        assert_eq!(positions("make test  # run it"), vec!["make"]);
    }

    #[test]
    fn quoted_prose_arg_is_not_a_command() {
        // `git commit -m "... catenary diagnostics ..."` → one command `git`,
        // no inner command (bug 33 / 17 family — quoted prose stays an arg).
        let input = r#"git commit -m "... catenary diagnostics ...""#;
        assert_eq!(positions(input), vec!["git"]);
        let cmd = sole(input);
        assert_eq!(cmd.name.as_deref(), Some("git"));
        assert!(cmd.substitutions.is_empty());
    }

    #[test]
    fn single_quote_escape_idiom() {
        // `git commit -m 'it'\''s done'` → one command `git` (no `s` / `done`).
        // Bug 33a: the `'\''` close·escape·reopen idiom keeps the whole thing
        // one argument.
        let input = r"git commit -m 'it'\''s done'";
        assert_eq!(positions(input), vec!["git"]);
        let cmd = sole(input);
        // The message argument reassembles to the literal `it's done`.
        assert_eq!(cmd.argv, vec!["commit", "-m", "it's done"]);
    }

    #[test]
    fn for_loop_is_compound_with_inner_command() {
        // `for f in *.rs; do git add "$f"; done` → compound; body command `git`.
        let input = r#"for f in *.rs; do git add "$f"; done"#;
        let script = parse(input);
        // The `git` command position is recovered from the loop body.
        assert!(
            script.command_positions().contains(&"git".to_string()),
            "expected `git` in command positions, got {:?}",
            script.command_positions()
        );
        // At least one segment is flagged compound.
        assert!(
            script
                .pipelines
                .iter()
                .flat_map(|p| &p.commands)
                .any(|c| c.is_compound),
            "expected a compound segment in {script:#?}"
        );
        // `for` never reaches command position (the bug-33 class false denial).
        assert!(!script.command_positions().contains(&"for".to_string()));
        // The loop variable `f` is structure, not a command (ticket 04): only
        // the body command is surfaced, so the loop of an allowlisted command
        // is allowed rather than denied on `f`.
        assert_eq!(script.command_positions(), vec!["git"]);
    }

    #[test]
    fn for_loop_variable_and_list_are_not_commands() {
        // Neither the loop variable nor the bare iteration words are command
        // positions; only the `do` body is.
        let input = "for cargo in build test; do echo hi; done";
        assert_eq!(parse(input).command_positions(), vec!["echo"]);
    }

    #[test]
    fn select_loop_variable_is_not_a_command() {
        // `select` mirrors `for`: variable + list are structure.
        let input = "select x in a b c; do make test; done";
        assert_eq!(parse(input).command_positions(), vec!["make"]);
    }

    #[test]
    fn for_iteration_list_substitution_still_runs() {
        // A substitution in the iteration list runs during expansion, so its
        // command position is still surfaced.
        let input = "for f in $(rm x); do echo hi; done";
        assert_eq!(parse(input).command_positions(), vec!["rm", "echo"]);
    }

    #[test]
    fn trailing_amp_marks_pipeline_backgrounded() {
        // A bare backgrounding `&` is retained as the pipeline's `backgrounded`
        // bit (the only joining operator the parse keeps — ticket 04).
        let script = parse("make test &");
        assert_eq!(script.pipelines.len(), 1);
        assert!(script.pipelines[0].backgrounded);
        // A `;`-terminated or unterminated pipeline is not backgrounded.
        let script = parse("make test ; cargo build");
        assert!(script.pipelines.iter().all(|p| !p.backgrounded));
        // In `a & b`, only the detached `a` is backgrounded.
        let script = parse("make a & cargo b");
        assert_eq!(script.pipelines.len(), 2);
        assert!(script.pipelines[0].backgrounded);
        assert!(!script.pipelines[1].backgrounded);
    }

    #[test]
    fn backslash_newline_continuation_joins() {
        // `cmd \<newline>more` → one command `cmd` (bug 30: continuation joined,
        // no stray `\` command).
        let input = "cmd \\\nmore";
        let cmd = sole(input);
        assert_eq!(cmd.name.as_deref(), Some("cmd"));
        assert_eq!(cmd.argv, vec!["more"]);
        assert_eq!(positions(input), vec!["cmd"]);
        // The stray backslash never becomes its own command.
        assert!(!positions(input).contains(&"\\".to_string()));
    }

    #[test]
    fn backslash_newline_in_chain() {
        // The bug-30 repro: a `\`-continued `&&` chain joins into one logical
        // line — both `git` commands seen, no `\` token.
        let input = "git add . && \\\ngit commit -m x";
        assert_eq!(positions(input), vec!["git", "git"]);
        assert!(!positions(input).contains(&"\\".to_string()));
    }

    #[test]
    fn quoted_redirect_is_not_a_redirect() {
        // `echo "a > b"` → one command `echo`, no redirect (bug 33c).
        let input = r#"echo "a > b""#;
        let cmd = sole(input);
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["a > b"]);
        assert!(
            cmd.redirects.is_empty(),
            "quoted `>` must not be a redirect, got {:?}",
            cmd.redirects
        );
    }

    #[test]
    fn substitution_and_sequence_positions() {
        // `echo $(rm x); true` → command positions {echo, rm, true}.
        let input = "echo $(rm x); true";
        assert_eq!(positions(input), vec!["echo", "rm", "true"]);
    }

    // ── Structural / regression cases ────────────────────────────────────────

    #[test]
    fn real_redirect_is_captured() {
        // Bug 11: a real `>` to a file is a redirect (the gate denies it later).
        let cmd = sole("git status > out.txt");
        assert_eq!(cmd.name.as_deref(), Some("git"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Write);
        assert_eq!(cmd.redirects[0].target, "out.txt");
    }

    #[test]
    fn append_redirect_glued_to_target() {
        let cmd = sole("echo hi>>log");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Append);
        assert_eq!(cmd.redirects[0].target, "log");
    }

    #[test]
    fn fd_dup_is_a_dup_not_a_file_write() {
        let cmd = sole("make test 2>&1");
        assert_eq!(cmd.name.as_deref(), Some("make"));
        // The source fd `2` attaches to the redirect, not argv.
        assert_eq!(cmd.argv, vec!["test"]);
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::DupOut);
        assert_eq!(cmd.redirects[0].target, "1");
    }

    #[test]
    fn fd_numbered_file_redirect() {
        // `2>err.log` is a real file redirect on fd 2 — the `2` is not an arg.
        let cmd = sole("cargo build 2>err.log");
        assert_eq!(cmd.name.as_deref(), Some("cargo"));
        assert_eq!(cmd.argv, vec!["build"]);
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Write);
        assert_eq!(cmd.redirects[0].target, "err.log");
    }

    #[test]
    fn fd_numbered_read_redirect() {
        // `0<input` is a read redirect on fd 0 — the source fd attaches to the
        // redirect (not argv) and the `<` classifies as a read, not a write.
        // Exercises the fd-redirect dispatch's `<` arm (the `>` sibling is pinned
        // by `fd_numbered_file_redirect`).
        let cmd = sole("cat 0<input");
        assert_eq!(cmd.name.as_deref(), Some("cat"));
        assert!(
            cmd.argv.is_empty(),
            "the source fd `0` must not leak into argv"
        );
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Read);
        assert_eq!(cmd.redirects[0].target, "input");
    }

    #[test]
    fn newline_separates_commands() {
        // Bug 20: a bare newline is a list separator.
        let input = "make test\ncargo build";
        assert_eq!(positions(input), vec!["make", "cargo"]);
    }

    #[test]
    fn pipe_splits_into_stages() {
        let script = parse("cat f | grep x | wc -l");
        assert_eq!(script.pipelines.len(), 1);
        assert_eq!(script.pipelines[0].commands.len(), 3);
        assert_eq!(script.command_positions(), vec!["cat", "grep", "wc"]);
    }

    #[test]
    fn double_pipe_is_a_list_not_a_pipe() {
        let script = parse("a || b");
        assert_eq!(script.pipelines.len(), 2);
        assert_eq!(script.command_positions(), vec!["a", "b"]);
    }

    #[test]
    fn background_amp_separates() {
        let input = "make test & cargo build";
        assert_eq!(positions(input), vec!["make", "cargo"]);
    }

    #[test]
    fn env_var_prefix_skipped() {
        let cmd = sole("FOO=bar RUST_LOG=debug cargo build");
        assert_eq!(cmd.name.as_deref(), Some("cargo"));
        assert_eq!(cmd.argv, vec!["build"]);
    }

    #[test]
    fn path_stripped_command_name() {
        let cmd = sole("/usr/bin/git status");
        assert_eq!(cmd.name.as_deref(), Some("git"));
    }

    #[test]
    fn backtick_substitution_recurses() {
        // Bug 17: backticks inside double quotes are command substitution.
        let input = r#"echo "`rm x`""#;
        assert_eq!(positions(input), vec!["echo", "rm"]);
    }

    #[test]
    fn process_substitution_recurses() {
        let input = "diff <(sort a) <(sort b)";
        assert_eq!(positions(input), vec!["diff", "sort", "sort"]);
    }

    #[test]
    fn hash_after_metacharacter_starts_comment() {
        // Bug 40 (found by the differential fuzz oracle): a `#` glued to the `)`
        // closing a process substitution begins a comment, exactly as the shell
        // does — so the trailing `#<(sort)` is discarded, not parsed as a second,
        // live process substitution. Without the fix our parser over-counts the
        // inner `sort` a second time (`["didiff","sort","sort"]`), a false denial.
        let input = "didiff <(sort a)#<(sort)";
        assert_eq!(positions(input), vec!["didiff", "sort"]);
    }

    #[test]
    fn hash_after_command_substitution_close_starts_comment() {
        // The same rule for `$(…)`: its closing `)` is a word-terminating
        // metacharacter, so `#bar` after it is a comment.
        assert_eq!(positions("echo $(date)#bar"), vec!["echo", "date"]);
    }

    #[test]
    fn hash_glued_mid_word_stays_literal() {
        // A `#` not at a word start is ordinary text — `cmd#x` is one word, and a
        // `#` directly after a `\`-newline join (no separating space) glues onto
        // the word rather than starting a comment (`echo\<nl>#x` → `echo#x`).
        assert_eq!(positions("cmd#x"), vec!["cmd#x"]);
        let cmd = sole("echo\\\n#x");
        assert_eq!(cmd.name.as_deref(), Some("echo#x"));
        assert!(cmd.argv.is_empty());
    }

    #[test]
    fn hash_inside_quotes_stays_literal() {
        // A `#` inside single/double quotes after a metacharacter is still
        // literal — the quote arms consume it before the comment check.
        let cmd = sole("echo '(#)' \"a#b\"");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["(#)", "a#b"]);
    }

    #[test]
    fn nested_substitution() {
        let input = "echo $(echo $(rm x))";
        assert_eq!(positions(input), vec!["echo", "echo", "rm"]);
    }

    #[test]
    fn heredoc_body_is_not_commands() {
        let input = "cat <<EOF\nrm -rf /\nEOF\ncargo build";
        // The heredoc body `rm -rf /` is literal; only `cat` and `cargo` run.
        assert_eq!(positions(input), vec!["cat", "cargo"]);
    }

    #[test]
    fn heredoc_quoted_delimiter_body_is_stripped() {
        // `<<'EOF'` (quoted delimiter): the body is still literal stdin, so a
        // command-looking line inside it never reaches the gate — only `cat`
        // and the trailing `make` run.
        let input = "cat <<'EOF'\nrm -rf /\nEOF\nmake test";
        assert_eq!(positions(input), vec!["cat", "make"]);
    }

    #[test]
    fn heredoc_dash_indented_terminator_closes() {
        // `<<-EOF` lets the closing delimiter (and body) be tab-indented; the
        // tab-indented `EOF` still terminates, so the body is stripped and the
        // command after it (`make`) splits out.
        let input = "cat <<-EOF\n\trm -rf /\n\tEOF\nmake test";
        assert_eq!(positions(input), vec!["cat", "make"]);
    }

    #[test]
    fn heredoc_delimiter_word_inside_body_does_not_close_early() {
        // A plain `<<EOF` closes only on a bare `EOF` at column 0. An *indented*
        // `EOF` is body text (the shell keeps reading), so it must not terminate
        // the heredoc early and leak the rest of the body as commands. Here the
        // indented `  EOF` is body; the real terminator is the column-0 `EOF`,
        // so the smuggled `rm -rf /` never surfaces.
        let input = "cat <<EOF\n  EOF\nrm -rf /\nEOF\nmake test";
        assert_eq!(positions(input), vec!["cat", "make"]);
    }

    #[test]
    fn heredoc_body_prose_with_metacharacters_is_stripped() {
        // The required bug-class case: prose in a commit-message heredoc body —
        // mentioning `catenary diagnostics` and carrying `;`/`&&` — is opaque
        // stdin, stripped before any gate sees it. Only `git` runs.
        let input = "git commit -F - <<EOF\nran catenary diagnostics; tidied && shipped\nEOF";
        assert_eq!(positions(input), vec!["git"]);
    }

    #[test]
    fn unquoted_heredoc_backtick_substitution_is_a_command() {
        // Bug 46: an *unquoted* `<<EOF` body is not opaque stdin — the shell
        // expands `` `…` `` in it and runs it. The smuggled `sed --in-place` is
        // a real command position on the `cat` that owns the heredoc, so both
        // `cat` and `sed` must surface (the `cat`/`sed` ordering follows the
        // marker-line append).
        let input = "cat <<EOF\n`sed --in-place 's/a/b/' f`\nEOF";
        assert_eq!(positions(input), vec!["cat", "sed"]);
        // The owning `cat` still carries its heredoc read redirect.
        let cmd = &parse(input).pipelines[0].commands[0];
        assert_eq!(cmd.name.as_deref(), Some("cat"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Read);
    }

    #[test]
    fn unquoted_heredoc_dollar_paren_substitution_is_a_command() {
        // The `$(…)` form of bug 46, and the prose around it stays inert — only
        // the substitution span projects, not the surrounding body words.
        let input = "cat <<EOF\nresult: $(rg foo) done\nEOF";
        assert_eq!(positions(input), vec!["cat", "rg"]);
    }

    #[test]
    fn unquoted_heredoc_projects_every_substitution() {
        // Multiple substitutions across multiple body lines all project, in
        // document order, after the owning command.
        let input = "cat <<EOF\n$(rg a)\nmid `sed -i s/x/y/ f`\nEOF\nmake test";
        assert_eq!(positions(input), vec!["cat", "rg", "sed", "make"]);
    }

    #[test]
    fn quoted_heredoc_delimiter_keeps_substitution_inert() {
        // A *quoted* delimiter (`<<'EOF'` / `<<"EOF"`) performs no expansion, so
        // a `$(…)` / `` `…` `` in the body is literal stdin — the smuggled `sed`
        // never reaches the gate. Counterpart to the unquoted cases above.
        let single = "cat <<'EOF'\n$(sed -i s/a/b/ f)\nEOF";
        assert_eq!(positions(single), vec!["cat"]);
        let double = "cat <<\"EOF\"\n`sed -i s/a/b/ f`\nEOF";
        assert_eq!(positions(double), vec!["cat"]);
    }

    #[test]
    fn escaped_heredoc_delimiter_keeps_substitution_inert() {
        // A backslash-escaped delimiter (`<<\EOF`) is also non-expanding, so the
        // body stays literal — same inert behavior as the quoted forms.
        let input = "cat <<\\EOF\n$(sed -i s/a/b/ f)\nEOF";
        assert_eq!(positions(input), vec!["cat"]);
    }

    #[test]
    fn unquoted_heredoc_escaped_dollar_is_inert() {
        // Inside the body the backslash defuses the substitution (`\$(…)` is a
        // literal `$(…)`), so no command is projected — only `cat` runs.
        let input = "cat <<EOF\n\\$(sed -i s/a/b/ f)\nEOF";
        assert_eq!(positions(input), vec!["cat"]);
    }

    #[test]
    fn unquoted_heredoc_arithmetic_is_not_a_command() {
        // `$((…))` is arithmetic expansion, not a command substitution — it runs
        // nothing, so it is skipped and only `cat` surfaces.
        let input = "cat <<EOF\ntotal=$((1 + 2))\nEOF";
        assert_eq!(positions(input), vec!["cat"]);
    }

    #[test]
    fn unquoted_heredoc_dash_marker_projects_substitution() {
        // `<<-EOF` strips leading tabs but is still an unquoted (expanding)
        // delimiter, so its body substitution projects.
        let input = "cat <<-EOF\n\t$(rg foo)\n\tEOF\nmake test";
        assert_eq!(positions(input), vec!["cat", "rg", "make"]);
    }

    #[test]
    fn brackets_inside_double_quotes_stay_arg() {
        // Bug 33b: bracketed text inside double quotes stays one argument.
        let input = r#"git commit -m "no [predicates] table""#;
        assert_eq!(positions(input), vec!["git"]);
        let cmd = sole(input);
        assert_eq!(cmd.argv, vec!["commit", "-m", "no [predicates] table"]);
    }

    #[test]
    fn quoted_semicolon_does_not_split() {
        let input = r#"echo "a; b""#;
        assert_eq!(positions(input), vec!["echo"]);
    }

    #[test]
    fn empty_input_is_empty_script() {
        assert_eq!(parse(""), ParsedScript::default());
        assert_eq!(parse("   \n  # just a comment"), ParsedScript::default());
    }

    #[test]
    fn while_loop_is_compound() {
        let input = "while read -r l; do echo \"$l\"; done";
        let script = parse(input);
        assert!(
            script
                .pipelines
                .iter()
                .flat_map(|p| &p.commands)
                .any(|c| c.is_compound)
        );
        assert!(!script.command_positions().contains(&"while".to_string()));
        assert!(!script.command_positions().contains(&"do".to_string()));
    }

    #[test]
    fn subshell_paren_is_compound() {
        let input = "(cd src && cargo build)";
        let script = parse(input);
        assert!(
            script
                .pipelines
                .iter()
                .flat_map(|p| &p.commands)
                .any(|c| c.is_compound)
        );
    }

    // ── Subshell trailing redirect (bug 45) ──────────────────────────────────
    //
    // A redirect *following* the closing `)` of a subshell binds to the whole
    // subshell. When the subshell carries no top-level list operator, its close
    // and the trailing redirect stay in the segment that began with `(`, which
    // routes through `collect_rest`. That sweep used to drop every `Token::Redir`
    // (it collected only words), so the bound redirect vanished — a SECURITY
    // under-count for the gate's `allow_file_redirects` enforcement. The fix
    // sweeps the redirects too; these cases pin it (the differential oracle, which
    // had been scoped to the brace-group form to dodge this bug, now also covers
    // the subshell form in `oracle.rs`).

    #[test]
    fn subshell_trailing_redirect_is_captured() {
        // Bug 45: `( echo hi ) > out` must surface the `OutputFile` redirect that
        // binds to the subshell — previously dropped (redirects `[]`).
        let cmd = sole("( echo hi ) > out");
        assert!(cmd.is_compound);
        assert_eq!(cmd.redirects.len(), 1, "got {:?}", cmd.redirects);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Write);
        assert_eq!(cmd.redirects[0].target, "out");
        // The inner `echo` is still a command position.
        assert!(
            parse("( echo hi ) > out")
                .command_positions()
                .contains(&"echo".to_string())
        );
    }

    #[test]
    fn subshell_trailing_append_redirect_is_captured() {
        // `( cmd ) >> out` — appending output redirect on the subshell.
        let cmd = sole("( cmd ) >> out");
        assert!(cmd.is_compound);
        assert_eq!(cmd.redirects.len(), 1, "got {:?}", cmd.redirects);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Append);
        assert_eq!(cmd.redirects[0].target, "out");
    }

    #[test]
    fn subshell_multiple_trailing_redirects_are_captured() {
        // `( cmd ) > out 2>&1` — a file write plus an fd duplication, both bound
        // to the subshell. The `2` is the source fd of the dup, not an argument.
        let cmd = sole("( cmd ) > out 2>&1");
        assert!(cmd.is_compound);
        assert_eq!(cmd.redirects.len(), 2, "got {:?}", cmd.redirects);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Write);
        assert_eq!(cmd.redirects[0].target, "out");
        assert_eq!(cmd.redirects[1].op, RedirectOp::DupOut);
        assert_eq!(cmd.redirects[1].target, "1");
    }

    #[test]
    fn nested_subshell_trailing_redirects_are_captured() {
        // `( ( a ) > x ) > y` — a redirect after each closing `)`. With no
        // top-level list operator the whole thing is one swept compound, so both
        // bound redirects must surface (matching brush's flat two-redirect
        // projection). The inner command `a` is still seen.
        let cmd = sole("( ( a ) > x ) > y");
        assert!(cmd.is_compound);
        assert_eq!(cmd.redirects.len(), 2, "got {:?}", cmd.redirects);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Write);
        assert_eq!(cmd.redirects[0].target, "x");
        assert_eq!(cmd.redirects[1].op, RedirectOp::Write);
        assert_eq!(cmd.redirects[1].target, "y");
        assert_eq!(parse("( ( a ) > x ) > y").command_positions(), vec!["a"]);
    }

    #[test]
    fn brace_group_trailing_redirect_still_captured() {
        // Regression guard for the reference form: `{ echo hi; } > out` still
        // surfaces its `OutputFile` redirect (the `;` splits the `} > out` tail
        // into its own simple command, where the redirect is captured). The fix
        // to the subshell arm must not perturb this.
        let script = parse("{ echo hi; } > out");
        let redirs: Vec<_> = script
            .pipelines
            .iter()
            .flat_map(|p| &p.commands)
            .flat_map(|c| &c.redirects)
            .collect();
        assert_eq!(redirs.len(), 1, "got {redirs:?}");
        assert_eq!(redirs[0].op, RedirectOp::Write);
        assert_eq!(redirs[0].target, "out");
        assert!(script.command_positions().contains(&"echo".to_string()));
    }

    // ── Scanning-loop boundary cases (lexer offset arithmetic) ───────────────
    //
    // The scanners below (`scan_balanced`, `scan_backtick`, `skip_double_quote`,
    // `lex_double_quote`, `lex_ansi_c_quote`, `lex_operator`) carry byte-offset
    // arithmetic whose edges decide where a quote / substitution / operator span
    // ends. An off-by-one there can mis-terminate a span and leak or swallow a
    // command, so these cases pin the exact span boundaries — not just that the
    // parse "ran".

    #[test]
    fn glued_fd_prefix_is_redirect_not_process_sub() {
        // `is_redir_fd_context`: a digit-run glued to `>(`/`<(` (`x2>(…)`) is an
        // fd redirect, NOT a process substitution — so it produces a redirect and
        // does NOT recurse. A bare `>(…)` (no digit prefix) is a process sub.
        let cmd = sole("echo x2>(cat f)");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Write);
        assert!(
            cmd.substitutions.is_empty(),
            "a digit-prefixed `>(` must not recurse, got {:?}",
            cmd.substitutions
        );
        // Contrast: bare `>(…)` IS a process substitution (recurses, no redirect).
        let bare = sole("echo >(cat f)");
        assert!(bare.redirects.is_empty());
        assert_eq!(bare.substitutions.len(), 1);
        assert_eq!(
            bare.substitutions[0].pipelines[0].commands[0]
                .name
                .as_deref(),
            Some("cat")
        );
    }

    #[test]
    fn substitution_word_is_just_the_substitution() {
        // `echo $(rm x)` — echo's only word is the substitution; the close index
        // lands exactly past the `)`, so no stray `)` or trailing byte leaks into
        // echo's argv, and `; true` is a separate top-level pipeline.
        let script = parse("echo $(rm x) ; true");
        assert_eq!(script.pipelines.len(), 2);
        let echo = &script.pipelines[0].commands[0];
        assert_eq!(echo.name.as_deref(), Some("echo"));
        assert_eq!(echo.argv, vec![""]);
        assert_eq!(echo.substitutions.len(), 1);
        assert_eq!(
            echo.substitutions[0].pipelines[0].commands[0]
                .name
                .as_deref(),
            Some("rm")
        );
        assert_eq!(
            script.pipelines[1].commands[0].name.as_deref(),
            Some("true")
        );
    }

    #[test]
    fn nested_substitution_outer_spans_to_own_close() {
        // depth tracking: the inner `$(rm x)` increments depth, so the FIRST `)`
        // closes only the inner sub; the outer `$( … )` spans its full body. `b`
        // is therefore captured INSIDE the substitution (an arg of the inner
        // command), and echo's only word is the substitution itself.
        let cmd = sole("echo $(a $(rm x) b)");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec![""]);
        assert_eq!(cmd.substitutions.len(), 1);
        let inner = &cmd.substitutions[0];
        assert_eq!(inner.pipelines.len(), 1);
        let a = &inner.pipelines[0].commands[0];
        assert_eq!(a.name.as_deref(), Some("a"));
        // The isolated inner `$(rm x)` is its own empty-text word, so `a`'s argv
        // is the substitution word (`""`) followed by `b`.
        assert_eq!(a.argv, vec!["", "b"]);
        assert_eq!(a.substitutions.len(), 1);
        assert_eq!(
            a.substitutions[0].pipelines[0].commands[0].name.as_deref(),
            Some("rm")
        );
    }

    #[test]
    fn nested_substitution_does_not_swallow_trailing_pipeline() {
        // The close mutants (`depth -= 1` → `+= / *=`) would never reach zero and
        // over-consume to EOF, swallowing `; true` into the substitution. The
        // outer must close at its own `)`, keeping `true` a top-level pipeline.
        let script = parse("echo $(a $(rm x) b) ; true");
        assert_eq!(script.pipelines.len(), 2);
        assert_eq!(
            script.pipelines[1].commands[0].name.as_deref(),
            Some("true")
        );
        assert_eq!(script.command_positions(), vec!["echo", "a", "rm", "true"]);
    }

    #[test]
    fn substitution_single_quote_span_exact() {
        // The single-quoted `')x'` protects `)` from closing the `$(…)`; the sub
        // closes at its own `)`, so `; true` is a separate top-level pipeline and
        // grep's quoted arg is intact (the `;` inside the quote does not split).
        let script = parse("echo $(grep ')x' f) ; true");
        assert_eq!(script.pipelines.len(), 2);
        assert_eq!(
            script.pipelines[1].commands[0].name.as_deref(),
            Some("true")
        );
        let echo = &script.pipelines[0].commands[0];
        assert_eq!(echo.substitutions.len(), 1);
        let grep = &echo.substitutions[0].pipelines[0].commands[0];
        assert_eq!(grep.name.as_deref(), Some("grep"));
        assert_eq!(grep.argv, vec![")x", "f"]);
        assert_eq!(echo.substitutions[0].pipelines.len(), 1);
        assert_eq!(script.command_positions(), vec!["echo", "grep", "true"]);
    }

    #[test]
    fn substitution_double_quote_span_exact() {
        // A `)` inside `"…"` in a `$(…)` must not close it (`skip_double_quote`);
        // the sub closes at its own `)`, leaving `; true` a separate pipeline.
        let script = parse(r#"echo $(grep ")x" f) ; true"#);
        assert_eq!(script.pipelines.len(), 2);
        assert_eq!(
            script.pipelines[1].commands[0].name.as_deref(),
            Some("true")
        );
        let echo = &script.pipelines[0].commands[0];
        assert_eq!(echo.substitutions.len(), 1);
        let grep = &echo.substitutions[0].pipelines[0].commands[0];
        assert_eq!(grep.name.as_deref(), Some("grep"));
        assert_eq!(grep.argv, vec![")x", "f"]);
        assert_eq!(script.command_positions(), vec!["echo", "grep", "true"]);
    }

    #[test]
    fn substitution_double_quote_escaped_quote_does_not_close_early() {
        // `skip_double_quote` (inside `$(…)`): a `\"` inside the `"…"` is an
        // escaped quote, so the run spans to the real closing `"` and the `$(…)`
        // closes at its own `)`. The `\)` between would otherwise leak.
        let script = parse(r#"echo $(grep "a\")b" f) ; true"#);
        assert_eq!(script.pipelines.len(), 2);
        assert_eq!(
            script.pipelines[1].commands[0].name.as_deref(),
            Some("true")
        );
        let echo = &script.pipelines[0].commands[0];
        assert_eq!(echo.substitutions.len(), 1);
        let grep = &echo.substitutions[0].pipelines[0].commands[0];
        assert_eq!(grep.name.as_deref(), Some("grep"));
        // The double-quoted arg reassembles to `a")b` (escaped quote, then `)b`).
        assert_eq!(grep.argv, vec![r#"a")b"#, "f"]);
    }

    #[test]
    fn substitution_double_quote_unterminated_no_panic() {
        // `skip_double_quote` must not index past the end on an unterminated `"`
        // inside a `$(…)` (the `i < n` → `i <= n` boundary). Defined, panic-free.
        let cmd = sole(r#"echo $(grep "unterminated)"#);
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.substitutions.len(), 1);
    }

    #[test]
    fn substitution_backslash_escapes_close_paren() {
        // Inside `$(…)` a `\)` is an escaped paren, not a closing one — so the sub
        // spans to its real `)` and the `\)` stays literal in the inner word.
        let script = parse(r"echo $(printf a\)z) ; true");
        assert_eq!(script.pipelines.len(), 2);
        let echo = &script.pipelines[0].commands[0];
        assert_eq!(echo.argv, vec![""]);
        assert_eq!(echo.substitutions.len(), 1);
        let printf = &echo.substitutions[0].pipelines[0].commands[0];
        assert_eq!(printf.name.as_deref(), Some("printf"));
        assert_eq!(printf.argv, vec!["a)z"]);
        assert_eq!(script.command_positions(), vec!["echo", "printf", "true"]);
    }

    #[test]
    fn backtick_escaped_backtick_does_not_close_early() {
        // Inside `` `…` `` a `\`` is an escaped backtick, not a close; the sub
        // spans to the real closing backtick, and the trailing word stays an arg.
        let cmd = sole(r"echo `printf a\`b` tail");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["", "tail"]);
        assert_eq!(cmd.substitutions.len(), 1);
        let printf = &cmd.substitutions[0].pipelines[0].commands[0];
        assert_eq!(printf.name.as_deref(), Some("printf"));
        assert_eq!(printf.argv, vec!["a`b"]);
    }

    #[test]
    fn double_quote_escaped_quote_is_one_arg() {
        // `lex_double_quote`: `\"` inside a double-quoted run is an escaped quote
        // that stays inside the word (the run does not close early).
        let cmd = sole(r#"echo "a\"b""#);
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec![r#"a"b"#]);
    }

    #[test]
    fn double_quote_backslash_newline_is_continuation() {
        // `lex_double_quote`: a `\`-newline inside a double-quoted run is a line
        // continuation — both bytes drop, joining the run (`"a\<nl>b"` → `ab`).
        let cmd = sole("echo \"a\\\nb\"");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["ab"]);
    }

    #[test]
    fn ansi_c_quote_keeps_escaped_byte() {
        // `lex_ansi_c_quote`: best-effort keeps the byte AFTER the backslash, so
        // `$'a\nb'` reassembles to `anb` (the escaped `n`, not the `a` before it).
        let cmd = sole(r"echo $'a\nb'");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["anb"]);
    }

    #[test]
    fn ansi_c_quote_trailing_backslash_no_panic() {
        // A trailing backslash at the end of an unterminated `$'…` must not index
        // past the end (`lex_ansi_c_quote` guard `i + 1 < n`); kept literally.
        let cmd = sole(r"echo $'ab\");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec![r"ab\"]);
    }

    #[test]
    fn crlf_line_continuation_joins() {
        // `lex`: a `\`-CRLF line continuation (`\<CR><LF>`) drops all three bytes,
        // joining the lines — distinct from the `\`-LF case already covered.
        let cmd = sole("cmd \\\r\nmore");
        assert_eq!(cmd.name.as_deref(), Some("cmd"));
        assert_eq!(cmd.argv, vec!["more"]);
        assert_eq!(positions("cmd \\\r\nmore"), vec!["cmd"]);
    }

    #[test]
    fn multi_digit_fd_redirect() {
        // `digit_run_len`: a multi-digit source fd (`10>file`) is consumed whole
        // as the redirect's fd — the digits never leak into argv.
        let cmd = sole("cmd 10>file");
        assert_eq!(cmd.name.as_deref(), Some("cmd"));
        assert!(
            cmd.argv.is_empty(),
            "fd digits must not be argv: {:?}",
            cmd.argv
        );
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Write);
        assert_eq!(cmd.redirects[0].target, "file");
    }

    #[test]
    fn bare_assignment_is_no_command() {
        // `build_command`: a stage of only `VAR=value` assignments has no
        // command word — no command position is projected. The segment itself
        // is retained (name `None`) carrying the assignment, so the write
        // resolver can bind `$VAR` targets later in the line (ws38 ticket 01).
        let script = parse("FOO=bar");
        assert!(script.command_positions().is_empty());
        let cmd = sole("FOO=bar");
        assert_eq!(cmd.name, None);
        assert_eq!(cmd.assignments.len(), 1);
        assert_eq!(cmd.assignments[0].name, "FOO");
        assert_eq!(cmd.assignments[0].value, "bar");
    }

    #[test]
    fn bare_quoted_assignment_is_no_command() {
        // Bug 52: a `NAME=` assignment whose value is quoted is still an
        // assignment, not a command — the quoted value must not leak to command
        // position. The `=` is unquoted in every case here (the quote opens after
        // it), so `is_assignment_prefix` skips the whole word.
        assert!(
            positions("x='/a/b/zzz'").is_empty(),
            "quoted value leaked: {:?}",
            positions("x='/a/b/zzz'")
        );
        assert!(
            positions("x=/a/b/zzz").is_empty(),
            "unquoted value leaked: {:?}",
            positions("x=/a/b/zzz")
        );
        assert!(
            positions("x='\"/a/b/zzz\"'").is_empty(),
            "nested-quoted value leaked: {:?}",
            positions("x='\"/a/b/zzz\"'")
        );
        let json = "j='{\"jsonrpc\":\"2.0\",\"uri\":\"file:///home/u/ws/x\"}'";
        assert!(
            positions(json).is_empty(),
            "quoted JSON value leaked: {:?}",
            positions(json)
        );
    }

    #[test]
    fn quoted_assignment_value_skipped_finds_command() {
        // Bug 52: a leading assignment with a quoted value is skipped, and the
        // following word is recovered as the command.
        assert_eq!(positions("FOO='/a/b' make test"), vec!["make"]);
    }

    #[test]
    fn fully_quoted_word_is_command_not_assignment() {
        // Bug 52: a word quoted at or before the `=` (`'x=y'`) is a command
        // literal, not an assignment prefix — it surfaces as the command.
        assert_eq!(positions("'x=y'"), vec!["x=y"]);
    }

    #[test]
    fn quoted_path_argument_unchanged() {
        // Bug 52 regression guard: a quoted path *argument* (not an assignment)
        // is unaffected — only the command `printf` surfaces.
        assert_eq!(positions("printf '%s' '/a/b/zzz'"), vec!["printf"]);
    }

    #[test]
    fn stray_close_paren_is_skipped_in_command() {
        // A `)` with no matching `(` in command position is structure, not an
        // argument: `build_command`'s `Token::Paren(_)` arm skips it and the
        // command name is still recovered. Pins that arm's `idx += 1` skip — the
        // arm that also folds in the (unreachable) `Token::Control` case — so a
        // mutation of the advance is caught rather than surviving on a dead branch.
        let cmd = sole("echo )");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert!(cmd.argv.is_empty(), "the stray `)` must not leak into argv");
    }

    // ── Operator lexing (`lex_operator` offsets / guards) ────────────────────

    #[test]
    fn redirect_write_both_truncate() {
        // `&>` redirects both stdout and stderr (truncate).
        let cmd = sole("make test &>log");
        assert_eq!(cmd.name.as_deref(), Some("make"));
        assert_eq!(cmd.argv, vec!["test"]);
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::WriteBoth);
        assert_eq!(cmd.redirects[0].target, "log");
    }

    #[test]
    fn redirect_write_both_append() {
        // `&>>` redirects both stdout and stderr (append); the 3-byte operator is
        // consumed whole, so the target is the following word, not a stray `>`.
        let cmd = sole("make test &>>log");
        assert_eq!(cmd.name.as_deref(), Some("make"));
        assert_eq!(cmd.argv, vec!["test"]);
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::WriteBoth);
        assert_eq!(cmd.redirects[0].target, "log");
    }

    #[test]
    fn redirect_clobber_is_write() {
        // `>|` clobber is treated as a plain write; the `|` is part of the
        // operator, not a pipe, and the target is the following word.
        let cmd = sole("echo hi >|log");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["hi"]);
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Write);
        assert_eq!(cmd.redirects[0].target, "log");
    }

    #[test]
    fn here_string_triple_lt() {
        // `<<<word` is a single here-string operator; the target is `word`.
        let cmd = sole("cat <<<word");
        assert_eq!(cmd.name.as_deref(), Some("cat"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::HereString);
        assert_eq!(cmd.redirects[0].target, "word");
    }

    #[test]
    fn heredoc_operator_is_single_read_redirect() {
        // `<<EOF` is a single Read redirect; its target word is the delimiter
        // (the body is stripped before lexing). The two `<` are one operator.
        let cmd = sole("cat <<EOF\nbody\nEOF");
        assert_eq!(cmd.name.as_deref(), Some("cat"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Read);
        assert_eq!(cmd.redirects[0].target, "EOF");
    }

    #[test]
    fn double_lt_at_eof_no_panic() {
        // A `<<` with no following delimiter at end of input must not index past
        // the end (`i + 2 < n` boundary); it is a single Read with empty target.
        let cmd = sole("cat <<");
        assert_eq!(cmd.name.as_deref(), Some("cat"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Read);
        assert_eq!(cmd.redirects[0].target, "");
    }

    #[test]
    fn dup_in_redirect() {
        // `<&3` is an input fd duplication (DupIn), target `3` — not a Read
        // followed by a backgrounding `&`.
        let cmd = sole("cat <&3");
        assert_eq!(cmd.name.as_deref(), Some("cat"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::DupIn);
        assert_eq!(cmd.redirects[0].target, "3");
    }

    #[test]
    fn lt_between_words_is_not_here_string() {
        // `<a<b`: each `<` has a non-`<` next byte, so each is a plain Read — the
        // here-string check must read `bytes[i + 1]` / `bytes[i + 2]`, not
        // `bytes[i]`. Two Read redirects, targets `a` and `b`.
        let cmd = sole("cat <a<b");
        assert_eq!(cmd.name.as_deref(), Some("cat"));
        assert_eq!(cmd.argv, Vec::<String>::new());
        assert_eq!(cmd.redirects.len(), 2);
        assert_eq!(cmd.redirects[0].op, RedirectOp::Read);
        assert_eq!(cmd.redirects[0].target, "a");
        assert_eq!(cmd.redirects[1].op, RedirectOp::Read);
        assert_eq!(cmd.redirects[1].target, "b");
    }

    // ── Multibyte UTF-8 in words (bug 43) ────────────────────────────────────
    //
    // The word accumulator buffers raw bytes and converts to text exactly once
    // at the word boundary, so a multibyte scalar fed one byte at a time is
    // never split across a `from_utf8_lossy` (which would replace each
    // lead/continuation byte with U+FFFD). These cases assert the *exact* word
    // text round-trips, so a regression to per-byte conversion is caught.

    #[test]
    fn unquoted_multibyte_word_roundtrips() {
        // `echo —x` (em-dash `—` = E2 80 94) must parse to argv `["—x"]`, not
        // the corrupted `["\u{FFFD}\u{FFFD}\u{FFFD}x"]` (bug 43).
        let cmd = sole("echo —x");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["—x"]);
        assert!(
            !cmd.argv[0].contains('\u{FFFD}'),
            "multibyte word must not be corrupted to U+FFFD, got {:?}",
            cmd.argv
        );
    }

    #[test]
    fn double_quoted_multibyte_word_roundtrips() {
        // A multibyte scalar inside a double-quoted word takes the
        // `lex_double_quote` byte path; it must round-trip intact.
        let cmd = sole(r#"echo "café—™""#);
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["café—™"]);
        assert!(!cmd.argv[0].contains('\u{FFFD}'));
    }

    #[test]
    fn backslash_escaped_multibyte_char_roundtrips() {
        // A `\`-escaped multibyte char keeps the following *scalar* literally:
        // the backslash arm feeds only the first continuation byte to
        // `push_byte`, and the remaining continuation bytes flow through the
        // ordinary-word arm — all into the same byte buffer, converted once.
        let cmd = sole(r"echo \—x");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["—x"]);
        assert!(!cmd.argv[0].contains('\u{FFFD}'));
    }

    #[test]
    fn ansi_c_quoted_multibyte_word_roundtrips() {
        // The `$'…'` path (`lex_ansi_c_quote`) is also byte-fed; a multibyte
        // scalar inside it must survive.
        let cmd = sole("echo $'—x'");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["—x"]);
        assert!(!cmd.argv[0].contains('\u{FFFD}'));
    }

    #[test]
    fn multibyte_command_name_roundtrips() {
        // A non-ASCII command *name* must not be mangled before allowlist
        // matching (gate-impact path noted in bug 43).
        let cmd = sole("café build");
        assert_eq!(cmd.name.as_deref(), Some("café"));
        assert_eq!(cmd.argv, vec!["build"]);
    }

    // ── End-of-input / mid-word edge guards (Sequence-7 mutant hardening) ─────
    //
    // The arms below pin guards whose only divergent input is an operator /
    // quote / escape sitting at — or one byte from — end of input, or a literal
    // metacharacter glued mid-word. Each case is constructed so the targeted
    // guard mutation changes the observed argv / redirect (or trips an
    // out-of-bounds index), so a passing assertion here fails under the mutant.

    #[test]
    fn literal_close_paren_mid_word_stays_in_word() {
        // `lex` paren arm guard `!in_word` (mutant 331:28 → `true`): a `)` that
        // is *not* at a word boundary (mid-word, `in_word` set) is an ordinary
        // byte, not a `Paren` token. With the guard forced true the `)` would
        // flush `a` and split the word, yielding argv `["a", "b"]` instead of the
        // single argument `a)b`.
        let cmd = sole("echo a)b");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["a)b"]);
    }

    #[test]
    fn trailing_backslash_at_eof_kept_literally() {
        // `lex` backslash arm guard `i + 1 < n` (mutant 266:26 → `i + 1 <= n`):
        // a lone trailing `\` has no following byte, so the continuation /
        // CRLF / escape checks must all fall through to the literal branch. The
        // `<=` mutant would read `bytes[i + 1]` past the end (out-of-bounds).
        let cmd = sole(r"echo \");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec![r"\"]);
    }

    #[test]
    fn backslash_then_lone_cr_at_eof_is_escaped_cr() {
        // `lex` CRLF-continuation guard `i + 2 < n` (mutant 270:33 →
        // `i + 2 <= n`): a `\` followed by a lone `\r` at EOF is *not* a CRLF
        // continuation (no `\n` follows), so it is an escaped `\r`. The `<=`
        // mutant would read `bytes[i + 2]` past the end (out-of-bounds).
        let cmd = sole("echo \\\r");
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["\r"]);
    }

    #[test]
    fn unterminated_double_quote_trailing_backslash_no_oob() {
        // `lex_double_quote` backslash guard `i + 1 < n` (mutant 445:22 →
        // `i * 1 < n`, i.e. always-true in-loop): a trailing `\` inside an
        // unterminated `"…` has no byte to escape and is kept literally. The
        // `i * 1` mutant would read `bytes[i + 1]` past the end (out-of-bounds).
        let cmd = sole(r#"echo "a\"#);
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec![r"a\"]);
    }

    #[test]
    fn unterminated_double_quote_trailing_dollar_no_oob() {
        // `lex_double_quote` `$(` lookahead guard `i + 1 < n` (mutant 466:23 →
        // `i * 1 < n`, always-true in-loop): a trailing `$` inside an
        // unterminated `"…` is a literal `$` (no `(` follows). The `i * 1`
        // mutant would read `bytes[i + 1]` past the end (out-of-bounds).
        let cmd = sole(r#"echo "a$"#);
        assert_eq!(cmd.name.as_deref(), Some("echo"));
        assert_eq!(cmd.argv, vec!["a$"]);
    }

    #[test]
    fn write_both_op_at_eof_is_truncate_not_oob() {
        // `lex_operator` `&>>` lookahead guard `i + 2 < n` (mutant 521:26 →
        // `i + 2 <= n`): a bare `&>` at EOF is `WriteBoth` (truncate) with an
        // empty target — there is no third byte to read. The `<=` mutant would
        // read `bytes[i + 2]` past the end (out-of-bounds).
        let cmd = sole("cat &>");
        assert_eq!(cmd.name.as_deref(), Some("cat"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::WriteBoth);
        assert_eq!(cmd.redirects[0].target, "");
    }

    #[test]
    fn pipe_op_at_eof_is_single_pipe_not_oob() {
        // `lex_operator` `||` lookahead guard `i + 1 < n` (mutant 532:18 →
        // `i * 1 < n`, always-true since `i < n` in `lex_operator`): a trailing
        // bare `|` at EOF is a single `Pipe`; the empty second stage is dropped,
        // leaving one command. The `i * 1` mutant would read `bytes[i + 1]` past
        // the end (out-of-bounds).
        let script = parse("cat |");
        assert_eq!(script.command_positions(), vec!["cat"]);
        assert_eq!(script.pipelines.len(), 1);
        assert_eq!(script.pipelines[0].commands.len(), 1);
    }

    #[test]
    fn here_string_short_target_is_single_here_string() {
        // `lex_operator` `<<<` length guard `i + 2 < n` (mutant 557:18 →
        // `i * 2 < n`): with a short target the operator sits where `i * 2 >= n`
        // while `i + 2 < n`, so the `*` mutant misreads `<<<` as a `<<` heredoc
        // plus a stray `<`, producing two `Read` redirects. The correct parse is
        // a single `HereString` whose target is the glued word. (`cat <<<x`: `<`
        // at index 4, n == 8, so `4 * 2 == 8` is *not* `< 8`.)
        let cmd = sole("cat <<<x");
        assert_eq!(cmd.name.as_deref(), Some("cat"));
        assert_eq!(cmd.redirects.len(), 1);
        assert_eq!(cmd.redirects[0].op, RedirectOp::HereString);
        assert_eq!(cmd.redirects[0].target, "x");
    }
}
