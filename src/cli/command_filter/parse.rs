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
//! line continuations (bug 30), removes heredoc bodies, and recurses into
//! `$(…)` / `` `…` `` / `<(…)` / `>(…)` substitutions.
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
    /// The redirections attached to this command.
    pub(crate) redirects: Vec<Redirect>,
    /// Command substitutions (`$(…)`, `` `…` ``, `<(…)`, `>(…)`) found anywhere
    /// in this command's words, each recursively parsed.
    pub(crate) substitutions: Vec<ParsedScript>,
    /// Whether this segment is a compound command — wrapped by a reserved word
    /// (`for`/`while`/`until`/`if`/`case`/`{`) or a `(` subshell. The parser
    /// only *recognizes* compounds; policy is the caller's (ticket 04).
    pub(crate) is_compound: bool,
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
    // line is kept so the redirection (`<<EOF`) is still seen.
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

    // The in-progress word, accumulated across quoted and unquoted runs.
    let mut word = String::new();
    let mut subs: Vec<String> = Vec::new();
    let mut had_quote = false;
    let mut in_word = false;

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
                &mut in_word,
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
                    push_byte(&mut word, bytes[i + 1]);
                    i += 2;
                } else {
                    // Trailing backslash at end of input — keep literally.
                    in_word = true;
                    word.push('\\');
                    i += 1;
                }
            }
            // ── Single quotes ───────────────────────────────────────────────
            b'\'' => {
                in_word = true;
                had_quote = true;
                let end = memchr_byte(bytes, b'\'', i + 1).unwrap_or(n);
                push_bytes(&mut word, &bytes[i + 1..end.min(n)]);
                i = if end < n { end + 1 } else { n };
            }
            // ── Double quotes ───────────────────────────────────────────────
            b'"' => {
                in_word = true;
                had_quote = true;
                i = lex_double_quote(bytes, i + 1, &mut word, &mut subs);
            }
            // ── `$` — `$'…'`, `$(…)`, or a plain `$word` ────────────────────
            b'$' => {
                in_word = true;
                if i + 1 < n && bytes[i + 1] == b'\'' {
                    // `$'…'` ANSI-C quoting.
                    had_quote = true;
                    i = lex_ansi_c_quote(bytes, i + 2, &mut word);
                } else if i + 1 < n && bytes[i + 1] == b'(' {
                    // `$(…)` command substitution (or `$((…))` arithmetic).
                    let (inner, next) = scan_balanced(bytes, i + 2, b'(', b')');
                    subs.push(inner);
                    i = next;
                } else {
                    word.push('$');
                    i += 1;
                }
            }
            // ── Backtick command substitution ───────────────────────────────
            b'`' => {
                in_word = true;
                let (inner, next) = scan_backtick(bytes, i + 1);
                subs.push(inner);
                i = next;
            }
            // ── Process substitution `<(…)` / `>(…)` ────────────────────────
            b'<' | b'>' if i + 1 < n && bytes[i + 1] == b'(' && !is_redir_fd_context(bytes, i) => {
                in_word = true;
                let (inner, next) = scan_balanced(bytes, i + 2, b'(', b')');
                subs.push(inner);
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
                i = lex_operator(bytes, op_at, &mut tokens);
            }
            // ── Operators (only at a word boundary) ─────────────────────────
            b';' | b'&' | b'|' | b'<' | b'>' => {
                flush_word!();
                i = lex_operator(bytes, i, &mut tokens);
            }
            // ── Ordinary word byte ──────────────────────────────────────────
            _ => {
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
fn flush_word(
    tokens: &mut Vec<Token>,
    word: &mut String,
    subs: &mut Vec<String>,
    had_quote: &mut bool,
    in_word: &mut bool,
) {
    if *in_word {
        tokens.push(classify_word(WordTok {
            text: std::mem::take(word),
            subs: std::mem::take(subs),
            had_quote: *had_quote,
        }));
        *had_quote = false;
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
fn lex_double_quote(
    bytes: &[u8],
    start: usize,
    word: &mut String,
    subs: &mut Vec<String>,
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
                        push_byte(word, next);
                        i += 2;
                    } else {
                        word.push('\\');
                        i += 1;
                    }
                } else {
                    word.push('\\');
                    i += 1;
                }
            }
            b'`' => {
                let (inner, next) = scan_backtick(bytes, i + 1);
                subs.push(inner);
                i = next;
            }
            b'$' if i + 1 < n && bytes[i + 1] == b'(' => {
                let (inner, next) = scan_balanced(bytes, i + 2, b'(', b')');
                subs.push(inner);
                i = next;
            }
            other => {
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
fn lex_ansi_c_quote(bytes: &[u8], start: usize, word: &mut String) -> usize {
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

/// Lex a control / redirection operator at index `i`, pushing the corresponding
/// token. Returns the index just past the operator.
fn lex_operator(bytes: &[u8], i: usize, tokens: &mut Vec<Token>) -> usize {
    let n = bytes.len();
    match bytes[i] {
        b';' => {
            tokens.push(Token::Control(Control::Semi));
            i + 1
        }
        b'&' => {
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
        b'|' => {
            if i + 1 < n && bytes[i + 1] == b'|' {
                tokens.push(Token::Control(Control::OrOr));
                i + 2
            } else {
                tokens.push(Token::Control(Control::Pipe));
                i + 1
            }
        }
        b'>' => {
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
        b'<' => {
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
        _ => i + 1,
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

/// Remove heredoc bodies *and* their closing-delimiter lines, keeping the
/// marker line (e.g. `cat <<EOF`) intact so the `<<` redirection is still
/// lexed.
///
/// A heredoc body is literal stdin, never commands — stripping it before the
/// lexer is what keeps body prose (a `catenary diagnostics` named in a commit
/// message, a `;`/`&&` in a sentence) out of every gate. The terminator match
/// is shell-faithful so a delimiter-like word *inside* the body does not close
/// the heredoc early: a plain `<<EOF` closes only on a bare `EOF` at column 0,
/// while `<<-EOF` permits a tab-indented one. The quoted (`<<'EOF'`) and
/// indented (`<<-EOF`) marker forms are recognized by
/// [`HEREDOC_MARKER_RE`](super::patterns::HEREDOC_MARKER_RE).
fn strip_heredoc_bodies(input: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut skip_until: Option<HeredocClose> = None;
    for line in input.split('\n') {
        if let Some(close) = &skip_until {
            if close.closes(line) {
                skip_until = None;
            }
            continue;
        }
        out.push(line);
        if let Some(caps) = HEREDOC_MARKER_RE.captures(line)
            && let Some(m) = caps.get(1)
        {
            // `m.start()` follows the `<<` (and any `-`); a `<<-` marker has the
            // dash immediately before the captured delimiter run / its quote.
            let dash = caps
                .get(0)
                .is_some_and(|whole| whole.as_str().starts_with("<<-"));
            skip_until = Some(HeredocClose {
                marker: m.as_str().to_string(),
                dash,
            });
        }
    }
    out.join("\n")
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
            Token::Paren(_) => {
                idx += 1;
            }
            Token::Redir(op) => {
                // The next word (if any) is the redirect target.
                if let Some(t) = tokens.get(idx + 1).and_then(token_word) {
                    cmd.redirects.push(Redirect {
                        op: *op,
                        target: t.text.clone(),
                    });
                    collect_subs(t, &mut cmd);
                    idx += 2;
                } else {
                    cmd.redirects.push(Redirect {
                        op: *op,
                        target: String::new(),
                    });
                    idx += 1;
                }
            }
            Token::Word(w) => {
                words.push(w);
                collect_subs(w, &mut cmd);
                idx += 1;
            }
            Token::Control(_) => {
                // Control operators never appear inside a built stage.
                idx += 1;
            }
        }
    }

    assign_name_and_argv(&words, &mut cmd);

    if cmd.name.is_none()
        && cmd.argv.is_empty()
        && cmd.redirects.is_empty()
        && cmd.substitutions.is_empty()
        && !cmd.is_compound
    {
        return None;
    }
    Some(cmd)
}

/// After hitting a reserved word / `(`, sweep the remaining tokens for words
/// (to find inner command positions) and substitutions, ignoring further
/// structure — gate 04 owns compound-body policy; the parser only surfaces the
/// inner words and recursed substitutions.
fn collect_rest<'a>(tokens: &'a [Token], words: &mut Vec<&'a WordTok>, cmd: &mut SimpleCommand) {
    for tok in tokens {
        match tok {
            Token::Word(w) => {
                words.push(w);
                collect_subs(w, cmd);
            }
            Token::Redir(_) | Token::Reserved(_) | Token::Paren(_) | Token::Control(_) => {}
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

/// Assign the command name (after `VAR=` prefixes, path-stripped) and argv from
/// the collected words.
fn assign_name_and_argv(words: &[&WordTok], cmd: &mut SimpleCommand) {
    // Skip leading `VAR=value` assignment words to find the command position.
    let mut name_idx = None;
    for (i, w) in words.iter().enumerate() {
        // An assignment prefix is an unquoted `NAME=...`; a quoted word is never
        // an assignment prefix in command position.
        if !w.had_quote && ENV_VAR_RE.is_match(&w.text) {
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

/// Append a single byte (from a valid `&str`, scanned in order) to a string,
/// preserving multibyte UTF-8 sequences without `unsafe`.
fn push_byte(s: &mut String, b: u8) {
    if b < 0x80 {
        s.push(b as char);
    } else {
        // A continuation / lead byte: re-validate through the byte buffer. Since
        // the source bytes come from a valid `&str` consumed in order, the tail
        // forms valid UTF-8 once the full sequence is present.
        let mut bytes = std::mem::take(s).into_bytes();
        bytes.push(b);
        *s = String::from_utf8_lossy(&bytes).into_owned();
    }
}

/// Append a byte slice (a sub-run of a valid `&str`) to a string.
fn push_bytes(s: &mut String, bytes: &[u8]) {
    s.push_str(&String::from_utf8_lossy(bytes));
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
}
