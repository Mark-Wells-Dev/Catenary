// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shell command parser for allowlist-based command filtering.
//!
//! Checks Bash commands against a [`ResolvedCommands`] allowlist. Reimplements
//! all parsing logic from `scripts/constrained_bash.py` in Rust: pipeline
//! position tracking, subshell recursion, heredoc exception, quote-aware
//! splitting, env var prefix skipping, full path stripping, and subcommand
//! deny matching.

#[allow(
    clippy::expect_used,
    reason = "all patterns are string literals verified by tests — no user input"
)]
mod patterns {
    use regex::Regex;
    use std::sync::LazyLock;

    /// Matches `$(...)`, `<(...)`, and `` `...` `` substitutions for recursive checking.
    pub static SUBSHELL_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"\$\(([^)]*)\)|<\(([^)]*)\)|`([^`]*)`").expect("constant pattern")
    });

    /// Matches heredoc start markers: `<<EOF`, `<<'EOF'`, `<<"EOF"`, `<<-EOF`.
    pub static HEREDOC_MARKER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"<<-?\s*\\?['""]?(\w+)['""]?"#).expect("constant pattern"));

    /// Splits on sequential operators: `&&`, `||`, `;`.
    pub static SEQ_SPLIT_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s*(?:&&|\|\||;)\s*").expect("constant pattern"));

    /// Matches env var assignment prefix: `VAR=value`.
    pub static ENV_VAR_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z_0-9]*=").expect("constant pattern"));

    /// Echo separator between sequential operators.
    pub static ECHO_SEP_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(&&|\|\||;)\s*echo\s+(?:"[^"]*"|'[^']*')\s*(&&|\|\||;)"#)
            .expect("constant pattern")
    });
}
use patterns::{ECHO_SEP_RE, ENV_VAR_RE, HEREDOC_MARKER_RE, SEQ_SPLIT_RE, SUBSHELL_RE};

/// Faithful, hand-rolled shell parser core (`&str → ParsedScript`).
///
/// The new parse substrate from decision 020 / tokenizer ticket 01. Tickets
/// 02–04 re-point the allowlist evaluator and the redirect guard at it; ticket
/// 07 deletes the legacy scanners above. Landed here but not yet wired into the
/// gate, so its surface is exercised by its own unit tests for now.
pub(crate) mod parse;

/// Differential fuzzing oracle (tokenizer ticket 05): `brush-parser` reference +
/// `proptest`. Dev-only — gated `#[cfg(test)]`, so the reference parser never
/// enters the runtime / `cargo deny` runtime graph and never ships.
#[cfg(test)]
mod oracle;

use regex::Regex;

use crate::config::ResolvedCommands;

/// Replace quoted content (including delimiters) with spaces.
///
/// Preserves string length and character positions so that regex
/// matches on the masked string can be mapped back to the original.
/// Prevents operators inside quoted strings from being treated as
/// shell operators.
fn mask_quotes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    let n = bytes.len();
    let mut i = 0;

    while i < n {
        if bytes[i] == b'\'' {
            // Skip to after the closing quote, or to end if unterminated.
            i = memchr::memchr(b'\'', &bytes[i + 1..]).map_or(n, |offset| i + 2 + offset);
        } else if bytes[i] == b'"' {
            let mut j = i + 1;
            while j < n && bytes[j] != b'"' {
                if bytes[j] == b'\\' && j + 1 < n {
                    j += 1;
                }
                j += 1;
            }
            i = j + 1;
        } else {
            out[i] = bytes[i];
            i += 1;
        }
    }

    String::from_utf8(out).unwrap_or_else(|_| " ".repeat(n))
}

/// Split `cmd` on `sep_re`, ignoring matches inside quoted strings.
fn quote_aware_split<'a>(cmd: &'a str, sep_re: &Regex) -> Vec<&'a str> {
    let masked = mask_quotes(cmd);
    let mut parts = Vec::new();
    let mut last = 0;
    for m in sep_re.find_iter(&masked) {
        parts.push(&cmd[last..m.start()]);
        last = m.end();
    }
    parts.push(&cmd[last..]);
    parts
}

/// Split `cmd` on bare `|` (not `||`), ignoring operators inside quotes.
///
/// Rust's `regex` crate does not support lookahead/lookbehind, so this
/// uses character-level scanning on the quote-masked string instead.
fn pipe_split(cmd: &str) -> Vec<&str> {
    let masked = mask_quotes(cmd);
    let bytes = masked.as_bytes();
    let n = bytes.len();
    let mut parts = Vec::new();
    let mut last = 0;
    let mut i = 0;

    while i < n {
        if bytes[i] == b'|' {
            // Skip || (logical OR) — not a pipe
            if i + 1 < n && bytes[i + 1] == b'|' {
                i += 2;
                continue;
            }
            // Check this isn't the second | of a || we already skipped past
            if i > 0 && bytes[i - 1] == b'|' {
                i += 1;
                continue;
            }
            // Bare pipe: split here
            let end = cmd[last..i].trim_end().len() + last;
            parts.push(&cmd[last..end]);
            i += 1;
            while i < n && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            last = i;
            continue;
        }
        i += 1;
    }
    parts.push(&cmd[last..]);
    parts
}

/// Split `cmd` on bare backgrounding `&` (not `&&`, `&>`, `>&`, or `N>&`),
/// ignoring `&` inside quotes.
///
/// A bare `&` terminates a list element and runs it in the background, so it
/// is a command separator just like `;` — `make test & cargo build` runs both.
/// In bash grammar `&` binds *looser* than `|`, `&&`, and `||`, so this is the
/// **outermost** split: applied before sequential (`&&`/`||`/`;`) and pipe
/// splitting, so every command around a `&` is seen by the filter rather than
/// swallowed into the previous command's arguments. A piece is *backgrounded*
/// iff it is not the last piece (the trailing `&` detaches it).
///
/// Operator disambiguation mirrors the (now-removed) background detector: `&&`
/// is sequential, `&>` is a redirect, and `>&`/`N>&` are fd duplications —
/// none split. Quotes are masked first, so a `&` inside an argument (a commit
/// message, a sed program) is never a separator. Scans the masked string and
/// slices the original, like [`pipe_split`].
///
/// Only **top-level** `&` split: a `&` inside a `(…)` grouping (`$(…)`, `<(…)`,
/// `>(…)`, a plain subshell) or backticks is left in place, so a wrapped
/// catenary command (`$(catenary grep p & foo)`) stays intact for the
/// `SUBSHELL_RE` recursion to recognize and deny precisely, and the inner list
/// is checked by that recursion rather than by a mid-substitution slice.
///
/// Heredoc bodies are stripped before this runs, and `&` does not occur in
/// heredoc markers, so heredoc handling (e.g. the `git commit` heredoc form)
/// is untouched.
///
/// The other bare separator — a newline — is split by [`newline_split`], a
/// sibling pass applied right after this one. It lives in its own function
/// because a newline never backgrounds the piece before it, so it can't share
/// this function's piece-is-backgrounded bookkeeping.
fn background_split(cmd: &str) -> Vec<&str> {
    let masked = mask_quotes(cmd);
    let b = masked.as_bytes();
    let n = b.len();
    let mut parts = Vec::new();
    let mut last = 0;
    let mut i = 0;
    // Don't split inside a `(…)` grouping or backticks — that `&` separates an
    // inner list, handled by the substitution recursion, not a top-level one.
    let mut paren_depth: u32 = 0;
    let mut in_backtick = false;
    while i < n {
        match b[i] {
            b'`' => {
                in_backtick = !in_backtick;
                i += 1;
                continue;
            }
            b'(' if !in_backtick => {
                paren_depth += 1;
                i += 1;
                continue;
            }
            b')' if !in_backtick => {
                paren_depth = paren_depth.saturating_sub(1);
                i += 1;
                continue;
            }
            b'&' => {}
            _ => {
                i += 1;
                continue;
            }
        }
        // From here `b[i] == b'&'`. Skip it entirely while nested.
        if in_backtick || paren_depth > 0 {
            i += 1;
            continue;
        }
        // `&&` — sequential operator, not a split point.
        if i + 1 < n && b[i + 1] == b'&' {
            i += 2;
            continue;
        }
        // Second `&` of a `&&` we already stepped past.
        if i > 0 && b[i - 1] == b'&' {
            i += 1;
            continue;
        }
        // `&>` redirect.
        if i + 1 < n && b[i + 1] == b'>' {
            i += 2;
            continue;
        }
        // `>&` / `N>&` fd duplication.
        if i > 0 && b[i - 1] == b'>' {
            i += 1;
            continue;
        }
        // Bare top-level `&` — split here (the preceding piece is backgrounded).
        parts.push(&cmd[last..i]);
        last = i + 1;
        i += 1;
    }
    parts.push(&cmd[last..]);
    parts
}

/// Split `cmd` on a bare newline — the loosest list separator — ignoring
/// newlines inside quotes or within a `(…)`/backtick grouping.
///
/// In bash a newline terminates a list element exactly like `;`
/// (`make test\ncargo build` runs both), so a command on the next line must
/// reach the filter too. Mirrors [`background_split`]'s top-level discipline:
/// quotes are masked first, so a newline inside a quoted argument — or the
/// multi-line `git commit -m "$(cat <<'EOF'…)"` form, whose body newlines live
/// inside the `"…"` — is never a separator; and a newline inside a `(…)`
/// grouping or backticks is left in place so the [`SUBSHELL_RE`] recursion owns
/// that inner list (a wrapped catenary command stays intact for precise
/// denial). Unlike `&`, a newline never backgrounds the preceding element, so
/// this is a plain separator with none of `background_split`'s operator
/// disambiguation.
///
/// Safe only because [`strip_heredoc_bodies`] runs first and removes heredoc
/// bodies *and their closing-delimiter lines*: the only newlines reaching here
/// are genuine separators (or quoted/grouped ones, masked or skipped). Were a
/// stray closing `EOF` left on its own line, this would slice it into a bogus
/// command — which is exactly why the body-strip drops the delimiter line.
fn newline_split(cmd: &str) -> Vec<&str> {
    let masked = mask_quotes(cmd);
    let b = masked.as_bytes();
    let n = b.len();
    let mut parts = Vec::new();
    let mut last = 0;
    let mut i = 0;
    // Don't split inside a `(…)` grouping or backticks — those newlines separate
    // an inner list owned by the substitution recursion, not a top-level one.
    let mut paren_depth: u32 = 0;
    let mut in_backtick = false;
    while i < n {
        match b[i] {
            b'`' => in_backtick = !in_backtick,
            b'(' if !in_backtick => paren_depth += 1,
            b')' if !in_backtick => paren_depth = paren_depth.saturating_sub(1),
            b'\n' if !in_backtick && paren_depth == 0 => {
                parts.push(&cmd[last..i]);
                last = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    parts.push(&cmd[last..]);
    parts
}

/// Strip echo separators between sequential operators.
///
/// Agents insert `&& echo "---" &&` as visual separators. This replaces
/// those patterns with just the operators so they don't interfere with
/// command checking.
fn strip_echo_separators(s: &str) -> String {
    let mut result = s.to_string();
    loop {
        let next = ECHO_SEP_RE.replace(&result, "$1 $2").to_string();
        if next == result {
            break;
        }
        result = next;
    }
    result
}

/// Remove heredoc bodies *and their closing-delimiter lines*, keeping the
/// marker line (e.g. `cat <<EOF`) intact.
///
/// Heredoc bodies are literal text, not shell commands. Without stripping them,
/// the recursive subshell checker would parse their content as commands —
/// triggering false denials on natural language. The closing-delimiter line (a
/// bare `EOF`) is dropped too, not kept: once [`newline_split`] treats a
/// newline as a command separator, a surviving `EOF` line would slice out as a
/// bogus command and false-deny. Dropping it also lets a real command *after*
/// the heredoc (`cat <<EOF…EOF\ncargo build`) split out and reach the filter
/// rather than being glued onto the marker token. The marker line is preserved
/// so the `has_heredoc` stdin exception in [`check_command`] still fires.
fn strip_heredoc_bodies(cmd_string: &str) -> String {
    let mut result = Vec::new();
    let mut skip_until: Option<String> = None;

    for line in cmd_string.split('\n') {
        if let Some(ref marker) = skip_until {
            if line.trim() == marker {
                skip_until = None;
            }
            // Drop the body lines and the closing-delimiter line alike.
            continue;
        }
        result.push(line);
        if let Some(m) = HEREDOC_MARKER_RE.captures(line)
            && let Some(marker) = m.get(1)
        {
            skip_until = Some(marker.as_str().to_string());
        }
    }
    result.join("\n")
}

/// Skip leading environment variable assignments to find the command token index.
///
/// Returns the index of the first token that is not a `VAR=value` assignment,
/// or `None` if all tokens are assignments.
fn find_command(tokens: &[&str]) -> Option<usize> {
    tokens.iter().position(|t| !ENV_VAR_RE.is_match(t))
}

/// Whether `<<` is the first argument after a segment's command word.
///
/// The heredoc exception keys off this: a command whose first operand is a
/// heredoc marker reads stdin, not files, so the allowlist denial is
/// suppressed (`cat <<'EOF'…` commit pattern). Quoted arguments are masked to
/// whitespace by [`shell_split`], so `sed 's/foo/bar/' <<EOF` reads as
/// `["sed", "<<EOF"]` and still qualifies, while an unquoted operand before the
/// heredoc (`grep pattern <<EOF`) does not. The per-argument quote information
/// needed to recover this from the faithful parse is not carried on
/// [`SimpleCommand`](parse::SimpleCommand), so the heuristic stays on the raw
/// segment until ticket 03 folds the redirect surface onto the parse.
fn segment_has_leading_heredoc(segment: &str) -> bool {
    let tokens = shell_split(segment);
    let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let Some(cmd_idx) = find_command(&token_refs) else {
        return false;
    };
    token_refs
        .get(cmd_idx + 1)
        .is_some_and(|t| t.starts_with("<<"))
}

/// Split a string on whitespace, respecting single and double quotes.
fn shell_split(s: &str) -> Vec<String> {
    let masked = mask_quotes(s);
    let masked_bytes = masked.as_bytes();
    let mut tokens = Vec::new();
    let mut start = None;

    for (i, &b) in masked_bytes.iter().enumerate() {
        if b == b' ' || b == b'\t' {
            if let Some(s_idx) = start {
                tokens.push(&s[s_idx..i]);
                start = None;
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s_idx) = start {
        tokens.push(&s[s_idx..]);
    }

    tokens.into_iter().map(String::from).collect()
}

/// Device sinks allowed as redirect targets even in the deny state.
///
/// These don't write the working tree, so a redirect to one never threatens
/// batch completeness. Anything more exotic flips `allow_file_redirects`
/// rather than growing this set.
const DEVICE_SINKS: [&str; 3] = ["/dev/null", "/dev/stdout", "/dev/stderr"];

/// Whether a shell segment redirects output to a file target.
///
/// Scans the quote-masked segment for `>` redirect operators (`>`, `>>`,
/// `1>`, `2>`, `&>`, `>|`, `>&`), so a `>` inside quotes is ignored. Two
/// forms carry no file target and are not flagged: file-descriptor
/// duplications (`2>&1`, `>&2`, `>&-`) and output process substitution
/// (`>(cmd)`). The literal [`DEVICE_SINKS`] are allowed. Every other `>`
/// pointing at a target is a file write.
///
/// Closes the redirection write-bypass (`bugs/11`): a redirected write skips
/// the tracked Edit/Write path and would make the diagnostics batch lie. The
/// target is read from the original bytes — a quoted target masks to spaces
/// and reads as empty (or as the following operator), which denies, since a
/// quoted redirect still writes a file and the device-sink exception is only
/// spelled unquoted.
fn redirects_to_file(segment: &str) -> bool {
    let masked = mask_quotes(segment);
    let mbytes = masked.as_bytes();
    let bytes = segment.as_bytes();
    let n = mbytes.len();
    let mut i = 0;

    while i < n {
        if mbytes[i] != b'>' {
            i += 1;
            continue;
        }

        // Consume the operator: `>`, optional append `>`, optional clobber
        // `|`, optional `&` (fd-dup or `>&word`).
        let mut j = i + 1;
        if j < n && mbytes[j] == b'>' {
            j += 1;
        }
        if j < n && mbytes[j] == b'|' {
            j += 1;
        }
        let amp = j < n && mbytes[j] == b'&';
        if amp {
            j += 1;
        }

        // `>&<digit>` / `>&-` duplicates a descriptor — no file target.
        if amp && j < n && (mbytes[j].is_ascii_digit() || mbytes[j] == b'-') {
            i = j;
            continue;
        }

        // `>(cmd)` is output process substitution, not a file write.
        if j < n && mbytes[j] == b'(' {
            i = j;
            continue;
        }

        // Skip whitespace between the operator and its target.
        while j < n && (mbytes[j] == b' ' || mbytes[j] == b'\t') {
            j += 1;
        }

        // Read the target token, stopping at whitespace or a shell operator.
        let start = j;
        while j < n
            && !mbytes[j].is_ascii_whitespace()
            && !matches!(mbytes[j], b'|' | b'<' | b'>' | b';' | b'&')
        {
            j += 1;
        }
        let target = &bytes[start..j];

        if target.is_empty() || !DEVICE_SINKS.iter().any(|s| target == s.as_bytes()) {
            return true;
        }

        // Device sink — allowed. Keep scanning for other redirects.
        i = j;
    }

    false
}

/// Check whether a command is denied by the allowlist rules.
///
/// A command is denied if:
/// 1. It is not in `allow` or `pipeline` (and not a `build` tool).
/// 2. It is in `pipeline` but at pipe position 0.
/// 3. It is in `allow` but the specific subcommand is in `deny.<cmd>`.
/// 4. It is otherwise allowed but uses a flag in `deny_flags.<cmd>`.
///
/// The heredoc exception suppresses denial for commands reading from stdin.
/// Returns the denied command name and reason if denied, `None` if allowed.
///
/// `cmd` is a [`SimpleCommand`](parse::SimpleCommand) from the faithful parse
/// (ticket 02): its command-position `name` and `argv` come from the
/// quote-faithful tokenizer, so a substring of a quoted argument can never be
/// mistaken for a command word. The decision ladder — heredoc exception → build
/// tool → unconditional allow + denied subcommand/flags → pipeline with the
/// position-0 guard → default deny — is unchanged. A command with no
/// command-position word (`name == None`) is not validated.
fn check_against_allowlist(
    cmd: &parse::SimpleCommand,
    has_heredoc: bool,
    pipe_pos: usize,
    rules: &ResolvedCommands,
    cwd: Option<&std::path::Path>,
) -> Option<(String, DenialReason)> {
    // No command-position word (only assignments / redirects) — nothing to
    // validate.
    let name = cmd.name.as_deref()?;
    let argv = &cmd.argv;

    // Heredoc exception: command is reading from stdin, not files.
    if has_heredoc {
        return None;
    }

    // Build tool is always allowed (per-root lookup with default fallback).
    if rules.build_for_cwd(cwd).iter().any(|t| t == name) {
        if let Some(flag) = check_denied_flags(name, argv, rules) {
            return Some((format!("{name} {flag}"), DenialReason::DeniedFlag));
        }
        return None;
    }

    // Check if command is in the unconditional allow list.
    if rules.allow.contains(name) {
        // Check subcommand deny: e.g., git is allowed but `git grep` is denied.
        // The subcommand is the first argument after the command word.
        // Returns the full denied form (e.g., "git grep") for clear denial messages.
        if let Some(sub) = argv.first()
            && let Some(denied_subs) = rules.deny.get(name)
            && denied_subs.contains(sub)
        {
            return Some((format!("{name} {sub}"), DenialReason::DeniedSubcommand));
        }
        if let Some(flag) = check_denied_flags(name, argv, rules) {
            return Some((format!("{name} {flag}"), DenialReason::DeniedFlag));
        }
        return None;
    }

    // Check if command is in the pipeline list.
    if rules.pipeline.contains(name) {
        // Pipeline commands are only allowed mid-pipeline (not at position 0).
        if pipe_pos == 0 {
            return Some((name.to_string(), DenialReason::PipelinePosition));
        }
        if let Some(flag) = check_denied_flags(name, argv, rules) {
            return Some((format!("{name} {flag}"), DenialReason::DeniedFlag));
        }
        return None;
    }

    // Not in any allow list — denied.
    Some((name.to_string(), DenialReason::NotAllowed))
}

/// Scan arguments for denied flags.
///
/// Checks `argv` (the words after the command name, from the parse) against
/// `deny_flags.<name>`. Long flags with `=` are split (e.g.,
/// `--manifest-path=Cargo.toml` matches `--manifest-path`). Short flags
/// are matched as-is — no combined flag decomposition (`-rf` does not
/// match `-r`).
///
/// Returns the matched flag if found.
fn check_denied_flags(name: &str, argv: &[String], rules: &ResolvedCommands) -> Option<String> {
    let denied = rules.deny_flags.get(name)?;
    for token in argv {
        let flag = if token.starts_with("--") {
            token
                .split_once('=')
                .map_or(token.as_str(), |(flag, _)| flag)
        } else {
            token.as_str()
        };
        if denied.contains(flag) {
            return Some(flag.to_string());
        }
    }
    None
}

/// Why a command was denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialReason {
    /// Command is not in `allow`, `pipeline`, or `build`.
    NotAllowed,
    /// Command is in `pipeline` but was used at pipeline position 0.
    PipelinePosition,
    /// Command is allowed but the specific subcommand is denied.
    DeniedSubcommand,
    /// Command is allowed but a specific flag is denied.
    DeniedFlag,
    /// Command redirects output to a file target (`>`, `>>`, `&>`, `2>file`).
    /// A redirected write bypasses the tracked Edit/Write path.
    OutputRedirect,
}

/// Result of a command check that was denied.
#[derive(Debug)]
pub struct Denial {
    /// The denied command name (e.g., `"cargo"`, `"git grep"`).
    pub command: String,
    /// Why the command was denied.
    pub reason: DenialReason,
    /// Whether an unresolvable `cd` target (variable, command substitution)
    /// was encountered before the denied command. When `true`, the effective
    /// cwd may be stale and the denial may be a false positive.
    pub unresolved_cd: bool,
    /// The effective working directory at the point of denial, after resolving
    /// any `cd` commands earlier in the pipeline. Used for cwd-aware build
    /// guidance.
    pub effective_cwd: Option<std::path::PathBuf>,
}

/// Check all commands in a shell command string against the allowlist rules.
///
/// `cwd` is used for per-root `build` tool lookup. Pass `None` when no
/// working directory is available (falls back to the user-level default
/// build tool).
///
/// Returns a [`Denial`] for the first denied command, or `None` if all
/// commands are allowed.
pub fn check_command(
    cmd: &str,
    rules: &ResolvedCommands,
    cwd: Option<&std::path::Path>,
) -> Option<Denial> {
    let cmd_string = strip_heredoc_bodies(cmd);
    let cmd_string = strip_echo_separators(&cmd_string);

    // Track effective cwd across sequential segments for per-root
    // build tool resolution. Updated when `cd <path>` is encountered.
    let mut effective_cwd: Option<std::path::PathBuf> = cwd.map(std::path::PathBuf::from);
    let mut saw_unresolved_cd = false;

    // Split on the loose list separators first — bare `&` (backgrounding) and a
    // bare newline — so a command after either runs as its own list element and
    // is checked too (`make test & cargo build` and `make test\ncargo build`
    // both run both). Backgrounding a *foreign* command is fine; we only need
    // each command name to reach the allowlist.
    let sequential: Vec<&str> = background_split(&cmd_string)
        .into_iter()
        .flat_map(|bg| newline_split(bg))
        .flat_map(|nl| quote_aware_split(nl, &SEQ_SPLIT_RE))
        .collect();
    for seq in sequential {
        let stages = pipe_split(seq);
        for (pipe_pos, segment) in stages.iter().enumerate() {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }

            // Recursively check $(), <(), and `` substitutions.
            for m in SUBSHELL_RE.captures_iter(segment) {
                let inner = m
                    .get(1)
                    .or_else(|| m.get(2))
                    .or_else(|| m.get(3))
                    .map_or("", |g| g.as_str().trim());
                if let Some(denial) = check_command(inner, rules, effective_cwd.as_deref()) {
                    return Some(denial);
                }
            }

            // Command-position word comes from the faithful parse (ticket 02):
            // the segment has no top-level list/pipe operators (those were
            // split above), so it parses to at most one pipeline holding one
            // simple command. Its `name`/`argv` are quote-faithful — a
            // substring of a quoted argument can never reach command position
            // (the bug-33 false-deny fix). Substitutions are still recursed
            // above via `SUBSHELL_RE`, and the redirect/heredoc guards still
            // read the raw `segment` (ticket 03 folds them onto the parse).
            let parsed = parse::parse(segment);
            let Some(parsed_cmd) = parsed.pipelines.first().and_then(|p| p.commands.first()) else {
                continue;
            };
            let Some(name) = parsed_cmd.name.as_deref() else {
                continue;
            };

            // Catenary's own commands run under the canonical-form matcher
            // (regime 1, `analyze_catenary_command`), not the foreign
            // allowlist. Skip them here so a search chain's foreign segments
            // (e.g. `cd src && catenary grep p`) are still validated without
            // `catenary` itself tripping the denylist.
            if name == "catenary" {
                continue;
            }

            // Output redirection to a file bypasses the tracked Edit/Write
            // path, making the diagnostics batch incomplete. Deny it before
            // the allow/deny decision (and before the heredoc exception) so
            // neither an otherwise-allowed command nor the heredoc
            // short-circuit can carry a redirect through. Gated by
            // `allow_file_redirects`. Ticket 03 reads this off
            // `parsed_cmd.redirects`; until then the byte scanner runs on the
            // raw segment (temporary overlap).
            if !rules.allow_file_redirects && redirects_to_file(segment) {
                return Some(Denial {
                    command: name.to_string(),
                    reason: DenialReason::OutputRedirect,
                    unresolved_cd: saw_unresolved_cd,
                    effective_cwd,
                });
            }

            // Heredoc exception: only when `<<` is the first argument after
            // the command name. Quoted arguments (like sed patterns) are
            // invisible to this masked tokenization, so `sed 's/foo/bar/'
            // <<EOF` reads as `["sed", "<<EOF"]`. This prevents
            // `rm -rf target/ <<EOF` from bypassing the allowlist while
            // preserving the `cat <<'EOF'` commit pattern. The per-argument
            // quote information needed to reproduce this from the parse is not
            // carried on `SimpleCommand`, so the heuristic stays on the raw
            // segment for now.
            let has_heredoc = segment_has_leading_heredoc(segment);

            if let Some((denied, reason)) = check_against_allowlist(
                parsed_cmd,
                has_heredoc,
                pipe_pos,
                rules,
                effective_cwd.as_deref(),
            ) {
                return Some(Denial {
                    command: denied,
                    reason,
                    unresolved_cd: saw_unresolved_cd,
                    effective_cwd,
                });
            }

            // Track `cd` to update effective cwd for subsequent segments. The
            // target is the first parsed argument after the command word.
            if name == "cd"
                && let Some(target) = parsed_cmd.argv.first()
            {
                let resolved = resolve_cd_target(target, effective_cwd.as_deref());
                if is_unresolvable_cd_target(target) {
                    saw_unresolved_cd = true;
                }
                effective_cwd = resolved;
            }
        }
    }

    None
}

/// Whether a `cd` target contains patterns we can't resolve.
fn is_unresolvable_cd_target(target: &str) -> bool {
    target.starts_with('$')
        || target.starts_with('`')
        || target.contains("$(")
        || (target.starts_with('~') && target != "~" && !target.starts_with("~/"))
}

/// Resolve a `cd` target path against the current effective cwd.
///
/// Handles absolute paths, relative paths, and `~/path` expansion.
/// Returns `None` for unresolvable paths (variables, command substitutions,
/// `~user`).
fn resolve_cd_target(
    target: &str,
    effective_cwd: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    // Skip unresolvable patterns: variables, command substitutions, ~user
    if target.starts_with('$') || target.starts_with('`') || target.contains("$(") {
        return effective_cwd.map(std::path::PathBuf::from);
    }

    let path = if target == "~" {
        dirs::home_dir()?
    } else if let Some(rest) = target.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else if target.starts_with('~') {
        // ~user — can't resolve
        return effective_cwd.map(std::path::PathBuf::from);
    } else if std::path::Path::new(target).is_absolute() {
        std::path::PathBuf::from(target)
    } else {
        // Relative path — resolve against effective cwd
        let base = effective_cwd?;
        base.join(target)
    };

    // Normalize `.` and `..` components without touching the filesystem.
    // `canonicalize()` would fail on non-existent paths.
    Some(normalize_path(&path))
}

/// Normalize a path by resolving `.` and `..` components lexically.
///
/// Unlike `canonicalize()`, this does not touch the filesystem — it works
/// on non-existent paths. Does not resolve symlinks.
fn normalize_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut normalized = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {} // skip `.`
            std::path::Component::ParentDir => {
                normalized.pop(); // resolve `..`
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// Extract all command names from a shell command string.
///
/// Re-points at the faithful parse's command-position projection (ticket 02):
/// [`ParsedScript::command_positions`](parse::ParsedScript::command_positions)
/// returns every command-position word across all pipelines and recursed
/// substitutions, path-stripped and `VAR=`-skipped, with quoted-argument
/// substrings never mistaken for commands. Returns the bare command names
/// (e.g., `rm`, `cp`).
///
/// Used by editing enforcement to decide whether a Bash tool call contains
/// only filesystem-manipulation commands; the consumer treats the result as a
/// set, so the projection order (command word before its substitutions) is not
/// significant.
#[must_use]
pub fn extract_command_names(cmd: &str) -> Vec<String> {
    parse::parse(cmd).command_positions()
}

// ── Catenary command canonical-form matcher (ADR 013/014) ───────────────
//
// Catenary's own commands run under a *fail-closed canonical-form* regime
// (ADR 013), separate from the foreign allowlist: only recognized subcommands,
// in a bare canonical shape, split by correlation class.
//
// - `diagnostics`/`sed` (load-bearing, correlated) must be **bare** — the sole
//   command in the call — so their hook→CLI handoff (ticket 17) consumes fast.
// - `grep`/`glob` (stateless, self-scoping) may `cd`-prefix and `&&`/`;`/`||`
//   chain with allowlisted foreign commands, any count.
//
// Both classes reject output-ownership violations (pipe, redirect,
// command/process-substitution *wrapping*, backgrounding `&`) and deny with a
// pedagogical message naming the right tool/flag. The matcher only recognizes
// and classifies; it performs no IO. Foreign commands keep the allowlist regime
// ([`check_command`]).

/// Correlation class of a recognized catenary subcommand (ADR 013/014).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatenaryClass {
    /// `grep`/`glob` — stateless, self-scoping; may chain/`cd`, any count.
    Search,
    /// `diagnostics`/`sed` — load-bearing, correlated; bare only.
    Correlated,
    /// `editing start`/`roots`/`primer` — bare lifecycle/management.
    Lifecycle,
}

/// A recognized agent-facing catenary subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    Grep,
    Glob,
    /// Forward-registered (CLI command lands in ticket 08).
    Sed,
    /// `catenary diagnostics` — prints diagnostics for the edited files.
    Diagnostics,
    EditingStart,
    /// Retired: `editing stop` was renamed to `diagnostics` (ticket 05). Still
    /// recognized so a stray invocation gets a redirect, not a generic
    /// "unknown command".
    EditingStop,
    Roots,
    Primer,
    /// `catenary commands` — prints the allowed-command surface.
    Commands,
}

impl Sub {
    /// Correlation class governing the canonical-form rules.
    const fn class(self) -> CatenaryClass {
        match self {
            Self::Grep | Self::Glob => CatenaryClass::Search,
            Self::Sed | Self::Diagnostics | Self::EditingStop => CatenaryClass::Correlated,
            Self::EditingStart | Self::Roots | Self::Primer | Self::Commands => {
                CatenaryClass::Lifecycle
            }
        }
    }

    /// Display form for deny messages (the canonical subcommand words).
    const fn label(self) -> &'static str {
        match self {
            Self::Grep => "grep",
            Self::Glob => "glob",
            Self::Sed => "sed",
            Self::Diagnostics => "diagnostics",
            Self::EditingStart => "editing start",
            Self::EditingStop => "editing stop",
            Self::Roots => "roots",
            Self::Primer => "primer",
            Self::Commands => "commands",
        }
    }
}

/// Recognition outcome for the words following `catenary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recog {
    /// An agent-facing subcommand.
    Agent(Sub),
    /// A real catenary subcommand reserved for host hooks / interactive use.
    NotAgent,
    /// Not a recognized subcommand (typo, bare `catenary`, `$VAR`, …).
    Unknown,
}

/// Classify the tokens following the `catenary` command word.
///
/// Multi-word subcommands (`editing start`, `roots add`) are matched before bare
/// words. Quotes were masked away by the tokenizer, so a literal `catenary`
/// inside an argument cannot be read as the subcommand.
fn recognize_catenary_sub(rest: &[&str]) -> Recog {
    match (rest.first().copied(), rest.get(1).copied()) {
        (Some("editing"), Some("start")) => Recog::Agent(Sub::EditingStart),
        (Some("editing"), Some("stop")) => Recog::Agent(Sub::EditingStop),
        (Some("roots"), Some("add" | "rm" | "ls")) => Recog::Agent(Sub::Roots),
        (Some("grep"), _) => Recog::Agent(Sub::Grep),
        (Some("glob"), _) => Recog::Agent(Sub::Glob),
        (Some("sed"), _) => Recog::Agent(Sub::Sed),
        (Some("diagnostics"), _) => Recog::Agent(Sub::Diagnostics),
        (Some("primer"), _) => Recog::Agent(Sub::Primer),
        (Some("commands"), _) => Recog::Agent(Sub::Commands),
        (
            Some("hook" | "stop" | "debug" | "config" | "doctor" | "install" | "update" | "daemon"),
            _,
        ) => Recog::NotAgent,
        _ => Recog::Unknown,
    }
}

/// The basename of a segment's command word (env prefix + path stripped), or
/// `None` if the segment has no command word.
fn segment_command_name(seg: &str) -> Option<String> {
    let tokens = shell_split(seg);
    let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let idx = find_command(&token_refs)?;
    Some(
        std::path::Path::new(token_refs[idx])
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(token_refs[idx])
            .to_string(),
    )
}

/// If `seg`'s command word is `catenary`, classify its subcommand; else `None`.
fn recognize_segment_catenary(seg: &str) -> Option<Recog> {
    let tokens = shell_split(seg);
    let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let idx = find_command(&token_refs)?;
    let name = std::path::Path::new(token_refs[idx])
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(token_refs[idx]);
    if name != "catenary" {
        return None;
    }
    Some(recognize_catenary_sub(&token_refs[idx + 1..]))
}

/// One recognized occurrence of a `catenary` command within a bash call, with
/// the output-ownership context of its segment.
#[allow(
    clippy::struct_excessive_bools,
    reason = "orthogonal per-occurrence output-ownership flags; a state machine \
              would obscure the independent checks"
)]
struct CatenaryOcc {
    recog: Recog,
    /// Catenary is downstream of a pipe (`… | catenary X`).
    piped_in: bool,
    /// Catenary pipes into a downstream command — its basename, if any.
    piped_out: Option<String>,
    /// The segment redirects output to a file (non-device-sink).
    redirected: bool,
    /// The segment is backgrounded with `&`.
    backgrounded: bool,
    /// Catenary is *wrapped* in a `$()`/`<()`/backtick substitution.
    wrapped: bool,
    /// `catenary sed --in-place` (the write form). `--in-place` is a literal
    /// flag (no shell expansion), so it reads identically hook-side and
    /// CLI-side — the hook stages an identity handoff only for the write form.
    in_place: bool,
}

/// What the `PreToolUse` hook should do with a shell command, after recognizing
/// and validating any catenary command it contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatenaryAction {
    /// No catenary command — hand to the foreign allowlist / editing regime.
    NotCatenary,
    /// A catenary command in a non-canonical form (or an unrecognized
    /// subcommand). The string is the pedagogical deny reason.
    Deny(String),
    /// A bare, canonical `catenary editing start` — route to the IPC handler.
    EditingStart,
    /// A bare, canonical `catenary diagnostics` — stage the done-editing handoff
    /// (internal `pre-tool/editing-stop`), then allow the command to run.
    Diagnostics,
    /// A bare, canonical `catenary sed`. `--in-place` is the write form: the
    /// hook stages an identity-forward handoff (internal `pre-tool/sed`) so the
    /// daemon can attribute the runtime-changed set. A preview (`in_place =
    /// false`) is a stateless query — no handoff. Either way the command runs.
    Sed {
        /// Whether `--in-place` was passed (write vs preview).
        in_place: bool,
    },
    /// A canonical catenary command (`grep`/`glob`/`roots`/`primer`).
    /// `has_foreign` is true when the call also contains foreign segments (a
    /// search chain) whose allowlist must still be checked.
    Allow {
        /// Whether foreign segments are present and need allowlist validation.
        has_foreign: bool,
    },
}

/// Recognize, classify, and validate any `catenary` command in a shell call
/// (regime 1 of ADR 013). Pure — no IO. See [`CatenaryAction`].
///
/// The two regimes are split by correlation class: `grep`/`glob` may be `cd`-
/// prefixed and `&&`/`;`/`||`-chained with allowlisted foreign commands;
/// `diagnostics`/`sed`/`editing`/`roots`/`primer` must be the sole command in
/// the call. Both classes reject pipes, file redirects, substitution-wrapping,
/// and backgrounding. Unrecognized or non-agent subcommands are denied.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one linear recognize→classify→validate pass; splitting it would \
              scatter the occurrence-collection state across helpers"
)]
pub fn analyze_catenary_command(cmd: &str) -> CatenaryAction {
    let stripped = strip_heredoc_bodies(cmd);
    let stripped = strip_echo_separators(&stripped);

    let mut occs: Vec<CatenaryOcc> = Vec::new();
    let mut foreign_segments = 0usize;
    let mut total_segments = 0usize;
    // A foreign command inside an arg-substitution (`catenary grep "$(foo)"`)
    // is permitted by hygiene but still allowlist-checked (regime 2, via the
    // subshell recursion in `check_command`). Flag it so the caller runs that
    // check even when there is no top-level foreign segment.
    let mut inner_foreign_substitution = false;

    // Bare `&` is the loosest separator and detaches the piece before it. Split
    // on it first (outermost) so a catenary command after `&` is still seen
    // (`foo & catenary diagnostics`), and a catenary command *before* a `&` is
    // marked backgrounded. A piece is backgrounded iff it is not the last one.
    let bg_pieces = background_split(&stripped);
    let piece_count = bg_pieces.len();
    let sequential: Vec<(&str, bool)> = bg_pieces
        .into_iter()
        .enumerate()
        .flat_map(|(idx, bg)| {
            let backgrounded = idx + 1 < piece_count;
            // A newline within the piece separates further commands but never
            // backgrounds them — they inherit the piece's `&` backgrounding.
            newline_split(bg)
                .into_iter()
                .flat_map(|nl| quote_aware_split(nl, &SEQ_SPLIT_RE))
                .map(move |seq| (seq, backgrounded))
        })
        .collect();
    for (seq, backgrounded) in sequential {
        let stages = pipe_split(seq);
        let stage_count = stages.len();
        for (pipe_pos, stage_raw) in stages.iter().enumerate() {
            let stage = stage_raw.trim();
            if stage.is_empty() {
                continue;
            }

            // Catenary *wrapped* in a substitution inside this stage.
            for m in SUBSHELL_RE.captures_iter(stage) {
                let inner = m
                    .get(1)
                    .or_else(|| m.get(2))
                    .or_else(|| m.get(3))
                    .map_or("", |g| g.as_str().trim());
                if let Some(recog) = recognize_segment_catenary(inner) {
                    occs.push(CatenaryOcc {
                        recog,
                        piped_in: false,
                        piped_out: None,
                        redirected: false,
                        backgrounded: false,
                        wrapped: true,
                        in_place: false,
                    });
                } else if segment_command_name(inner).is_some() {
                    inner_foreign_substitution = true;
                }
            }

            // The stage's own command word.
            let Some(name) = segment_command_name(stage) else {
                continue;
            };
            if name == "catenary" {
                total_segments += 1;
                let recog = recognize_segment_catenary(stage).unwrap_or(Recog::Unknown);
                let piped_out = if pipe_pos + 1 < stage_count {
                    segment_command_name(stages[pipe_pos + 1])
                } else {
                    None
                };
                let in_place = matches!(recog, Recog::Agent(Sub::Sed))
                    && shell_split(stage).iter().any(|t| t == "--in-place");
                occs.push(CatenaryOcc {
                    recog,
                    piped_in: pipe_pos > 0,
                    piped_out,
                    // `background_split` already consumed every bare `&`, so the
                    // stage holds none; backgrounding is a property of the piece.
                    redirected: redirects_to_file(stage),
                    backgrounded,
                    wrapped: false,
                    in_place,
                });
            } else {
                foreign_segments += 1;
                total_segments += 1;
            }
        }
    }

    if occs.is_empty() {
        return CatenaryAction::NotCatenary;
    }

    // `catenary editing stop` is retired — renamed to `catenary diagnostics`
    // (ticket 05). Catch it in any form, before the output-ownership and
    // bare-only denials, so the agent always learns the new name rather than a
    // generic output-ownership complaint.
    if occs
        .iter()
        .any(|o| matches!(o.recog, Recog::Agent(Sub::EditingStop)))
    {
        return CatenaryAction::Deny(editing_stop_retired_denial());
    }

    // First occurrence with a per-command problem wins (document order).
    for occ in &occs {
        if let Some(msg) = catenary_occ_denial(occ) {
            return CatenaryAction::Deny(msg);
        }
    }

    // Every occurrence is a clean, agent-invocable command.
    let subs: Vec<Sub> = occs
        .iter()
        .filter_map(|o| match o.recog {
            Recog::Agent(s) => Some(s),
            Recog::NotAgent | Recog::Unknown => None,
        })
        .collect();

    let has_foreign = foreign_segments > 0 || inner_foreign_substitution;

    if subs.iter().any(|s| s.class() != CatenaryClass::Search) {
        // A correlated/lifecycle command must be bare: the only command anywhere
        // (an arg-substitution is not a separate segment, so it stays bare).
        if total_segments != 1 || occs.len() != 1 {
            return CatenaryAction::Deny(bare_only_denial(&subs));
        }
        return match subs.first() {
            Some(Sub::EditingStart) => CatenaryAction::EditingStart,
            Some(Sub::Diagnostics) => CatenaryAction::Diagnostics,
            // Bare-only already enforced above, so `occs` holds exactly the sed
            // occurrence — read `--in-place` off it to pick the handoff path.
            Some(Sub::Sed) => CatenaryAction::Sed {
                in_place: occs.first().is_some_and(|o| o.in_place),
            },
            _ => CatenaryAction::Allow { has_foreign },
        };
    }

    // All occurrences are search commands — chaining/`cd`/count all allowed.
    CatenaryAction::Allow { has_foreign }
}

/// Convenience for the editing-boundary defense (`hook_router`): the deny reason
/// when `cmd` is a non-canonical catenary command, else `None`.
#[must_use]
pub fn catenary_command_denial(cmd: &str) -> Option<String> {
    match analyze_catenary_command(cmd) {
        CatenaryAction::Deny(msg) => Some(msg),
        CatenaryAction::NotCatenary
        | CatenaryAction::EditingStart
        | CatenaryAction::Diagnostics
        | CatenaryAction::Sed { .. }
        | CatenaryAction::Allow { .. } => None,
    }
}

/// Per-occurrence deny reason in priority order, or `None` if the occurrence is
/// a clean, agent-invocable command.
fn catenary_occ_denial(occ: &CatenaryOcc) -> Option<String> {
    let sub = match occ.recog {
        Recog::Unknown => return Some(unknown_subcommand_denial()),
        Recog::NotAgent => return Some(not_agent_invocable_denial()),
        Recog::Agent(s) => s,
    };
    if occ.wrapped {
        return Some(substitution_denial(sub));
    }
    if occ.piped_in {
        return Some(stdin_denial(sub));
    }
    if let Some(down) = &occ.piped_out {
        return Some(out_pipe_denial(sub, down));
    }
    if occ.redirected {
        return Some(redirect_denial(sub));
    }
    if occ.backgrounded {
        return Some(background_denial(sub));
    }
    None
}

/// The recognized agent-facing command surface, for "unknown subcommand" denials.
const CATENARY_SURFACE: &str = "Available: `grep`, `glob`, `sed`, `diagnostics`, \
     `editing start`, `roots add/rm/ls`, `commands`, `primer`. Run `catenary primer` \
     for the workflow.";

fn unknown_subcommand_denial() -> String {
    format!("That isn't a recognized `catenary` command. {CATENARY_SURFACE}")
}

fn not_agent_invocable_denial() -> String {
    format!(
        "That `catenary` command is for host CLI hooks and interactive use, not \
         agents. {CATENARY_SURFACE}"
    )
}

fn substitution_denial(sub: Sub) -> String {
    format!(
        "Don't capture `catenary {}` output with `$(…)` or backticks — run it bare \
         and read the result directly.",
        sub.label()
    )
}

/// `… | catenary X` — catenary does not read stdin.
fn stdin_denial(sub: Sub) -> String {
    match sub {
        Sub::Grep => "`catenary grep` doesn't read stdin — it searches the filesystem. \
             Give it a glob pattern path and narrow with `--exclude-pattern`, e.g. \
             `catenary grep \"p\" 'src/**/*.rs' --exclude-pattern 'tests/**'`."
            .to_string(),
        Sub::Glob => "`catenary glob` doesn't read stdin — it browses the filesystem. \
             Give it a path or glob pattern, e.g. `catenary glob 'src/**/*.rs'`."
            .to_string(),
        Sub::Sed => "`catenary sed` takes `<pattern> <replacement> [paths]`, not stdin. \
             Pass a glob pattern path."
            .to_string(),
        Sub::Diagnostics => "`catenary diagnostics` takes no input — run it bare.".to_string(),
        Sub::EditingStart | Sub::EditingStop | Sub::Roots | Sub::Primer | Sub::Commands => {
            format!("`catenary {}` takes no stdin — run it bare.", sub.label())
        }
    }
}

/// `catenary X | downstream` — catenary owns its (structured, budgeted) output.
fn out_pipe_denial(sub: Sub, downstream: &str) -> String {
    let tool = sub.label();
    match sub {
        Sub::Grep | Sub::Glob => match downstream {
            "head" | "tail" => format!(
                "`catenary {tool}` output is paged — use `--page N` (default 1), not \
                 `{downstream}`."
            ),
            "wc" => format!(
                "Use `--count` for totals — piping `catenary {tool}` into `wc` also \
                 counts headers and context lines."
            ),
            "grep" | "egrep" | "fgrep" | "rg" | "ag" | "sort" | "uniq" | "cut" | "awk" | "sed" => {
                format!(
                    "`catenary {tool}` returns structured, enriched results — don't \
                 post-filter with `{downstream}`. Narrow the query: tighten the \
                 pattern or add `--exclude-pattern`."
                )
            }
            _ => format!(
                "`catenary {tool}` owns its output — don't pipe it into `{downstream}`. \
                 Size output with `--page`, narrow with `--exclude-pattern`, and read \
                 the result directly."
            ),
        },
        Sub::Sed => format!(
            "`catenary sed` output is structured (file list + match counts) — don't \
             pipe it into `{downstream}`. Use `--page N`; narrow with \
             `--exclude-pattern`. `--in-place` writes the files directly."
        ),
        Sub::Diagnostics => format!(
            "`catenary diagnostics` clears on run and writes the full report to a \
             runtime-dir file (path printed); the preview is already budgeted, errors \
             first. Don't pipe it into `{downstream}` — read the preview, or `catenary \
             grep \"pattern\" <report-file>` to filter the full set."
        ),
        Sub::EditingStart | Sub::EditingStop | Sub::Roots | Sub::Primer | Sub::Commands => {
            format!(
                "`catenary {tool}` owns its output — run it bare, don't pipe it into \
                 `{downstream}`."
            )
        }
    }
}

fn redirect_denial(sub: Sub) -> String {
    match sub {
        Sub::Grep | Sub::Glob => format!(
            "`catenary {}` results are printed for you to read — don't redirect them to \
             a file. Page large output with `--page N`.",
            sub.label()
        ),
        Sub::Sed => "`catenary sed` edits files directly with `--in-place` (or previews \
             to you) — there's nothing to redirect."
            .to_string(),
        Sub::Diagnostics => "`catenary diagnostics` already writes its full report to a \
             runtime-dir file (path printed) — run it bare and read the printed path."
            .to_string(),
        Sub::EditingStart | Sub::EditingStop | Sub::Roots | Sub::Primer | Sub::Commands => {
            format!(
                "`catenary {}` output is delivered to you directly — don't redirect it.",
                sub.label()
            )
        }
    }
}

fn background_denial(sub: Sub) -> String {
    format!(
        "Don't background `catenary {}` with `&` — its output is delivered to you \
         directly and backgrounding drops it. Run it in the foreground.",
        sub.label()
    )
}

/// Bare-only violation for a correlated/lifecycle command sharing the call.
fn bare_only_denial(subs: &[Sub]) -> String {
    let correlated = subs
        .iter()
        .filter(|s| s.class() == CatenaryClass::Correlated)
        .count();
    if correlated >= 2 || subs.len() >= 2 {
        return "Run each catenary command on its own line — `diagnostics`/`sed` (and \
             the editing lifecycle) can't share a command with anything else; they must \
             reach the daemon immediately to attribute correctly."
            .to_string();
    }
    let label = subs.first().map_or("diagnostics", |s| s.label());
    format!(
        "Run `catenary {label}` as its own command — it can't be combined with other \
         commands (no `cd` prefix, no `&&`/`;`/`||` chain). It must reach the daemon \
         promptly to attribute correctly; it takes no paths, so the working directory \
         doesn't matter."
    )
}

/// Redirect for the retired `catenary editing stop` — renamed to
/// `catenary diagnostics` (ticket 05).
fn editing_stop_retired_denial() -> String {
    "`catenary editing stop` is now `catenary diagnostics` — run `catenary \
     diagnostics` to print diagnostics for your edits."
        .to_string()
}

/// Top-level CLI command, set once at binary startup by [`set_cli_command`].
///
/// Used by [`render_subcommand_help`] to extract subcommand help text
/// for denial redirect messages and error-help-on-stderr. Library code
/// reads this; the binary sets it.
static CLI_COMMAND: std::sync::OnceLock<clap::Command> = std::sync::OnceLock::new();

/// Store the top-level CLI command for subcommand help rendering.
///
/// Called once from `main()` before any hook or command dispatch.
/// Subsequent calls are no-ops.
pub fn set_cli_command(cmd: clap::Command) {
    CLI_COMMAND.set(cmd).ok();
}

/// Render the `-h` output for a Catenary subcommand.
///
/// Looks up the named subcommand in the top-level CLI definition
/// (set via [`set_cli_command`]), sets the bin name, suppresses the
/// auto-generated `help` subcommand, and renders. Returns an empty
/// string if the CLI command is not set or the subcommand is not found.
fn render_subcommand_help(subcommand: &str) -> String {
    let Some(cli) = CLI_COMMAND.get() else {
        return String::new();
    };
    let Some(sub) = cli.find_subcommand(subcommand) else {
        return String::new();
    };
    let mut sub = sub.clone();
    sub = sub
        .bin_name(format!("catenary {subcommand}"))
        .disable_help_subcommand(true);
    sub.render_help().to_string()
}

/// Resolve per-client template variables in guidance messages.
///
/// `{READ}` and `{EDIT}` resolve to the host CLI's tool names.
#[allow(
    clippy::literal_string_with_formatting_args,
    reason = "{READ} and {EDIT} are template variables, not format args"
)]
fn resolve_client_vars(msg: &str, format: Option<super::HostFormat>) -> String {
    let (read, edit) = format.map_or(("Read", "Edit"), |f| (f.read_tool(), f.edit_tool()));
    msg.replace("{READ}", read).replace("{EDIT}", edit)
}

/// Denial message for output redirection to a file target.
///
/// A redirected write skips the host's edit tool, so post-edit diagnostics
/// can't observe it — the message routes the agent back through the tracked
/// path and names the `allow_file_redirects` escape hatch. Used by both the
/// full and short denial forms (it carries the same essential guidance).
fn format_redirect_denial(format: Option<super::HostFormat>) -> String {
    let edit = format.map_or("Edit", super::HostFormat::edit_tool);
    format!(
        "Output redirection to a file isn't allowed — a redirected write \
         bypasses the {edit} tool, so post-edit diagnostics can't see it. \
         Use {edit} to write files. (`2>&1`, `>&2`, and `/dev/null`-style \
         sinks are still allowed; set `allow_file_redirects = true` under \
         `[commands]` to permit file redirects.)"
    )
}

/// Format the opening line based on denial reason.
fn format_opening_line(denied_cmd: &str, reason: DenialReason) -> String {
    match reason {
        DenialReason::NotAllowed => {
            format!("`{denied_cmd}` isn't allowed by the current Catenary configuration.")
        }
        DenialReason::PipelinePosition => {
            format!("`{denied_cmd}` isn't allowed at the start of a pipeline.")
        }
        DenialReason::DeniedSubcommand => {
            format!("`{denied_cmd}` isn't allowed (denied subcommand).")
        }
        DenialReason::DeniedFlag => {
            format!("`{denied_cmd}` isn't allowed (denied flag).")
        }
        // OutputRedirect denials early-return in the callers; this arm only
        // satisfies exhaustiveness.
        DenialReason::OutputRedirect => {
            format!("`{denied_cmd}` isn't allowed (output redirection).")
        }
    }
}

/// One-line pointer to `catenary commands`, where the full allow / pipeline /
/// deny surface lives. Every denial carries this pointer instead of dumping the
/// whole surface inline (decision 023).
const SURFACE_POINTER: &str = "Run `catenary commands` for the allowed command surface.";

/// Render the allowed-command surface as sorted lines: `Allowed`, `Allowed in
/// pipelines`, `Denied subcommands`, and `Denied flags`. Sections with no
/// entries are omitted.
///
/// This is the canonical surface listing — `catenary commands` prints it and
/// denial messages point there rather than inlining it. Build tools are
/// deliberately excluded: per-cwd build context rides the denial's `Hint`
/// line, not this surface.
#[must_use]
pub fn format_command_surface(commands: &ResolvedCommands) -> Vec<String> {
    let mut parts = Vec::new();

    if !commands.allow.is_empty() {
        let mut sorted: Vec<&str> = commands.allow.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        parts.push(format!("Allowed: {}", sorted.join(", ")));
    }

    if !commands.pipeline.is_empty() {
        let mut sorted: Vec<&str> = commands.pipeline.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        parts.push(format!(
            "Allowed in pipelines (not first): {}",
            sorted.join(", ")
        ));
    }

    if !commands.deny.is_empty() {
        let mut denied_pairs: Vec<String> = Vec::new();
        for (cmd, subs) in &commands.deny {
            let mut sorted_subs: Vec<&str> = subs.iter().map(String::as_str).collect();
            sorted_subs.sort_unstable();
            for sub in sorted_subs {
                denied_pairs.push(format!("{cmd} {sub}"));
            }
        }
        denied_pairs.sort_unstable();
        parts.push(format!("Denied subcommands: {}", denied_pairs.join(", ")));
    }

    if !commands.deny_flags.is_empty() {
        let mut flag_pairs: Vec<String> = Vec::new();
        for (cmd, flags) in &commands.deny_flags {
            let mut sorted_flags: Vec<&str> = flags.iter().map(String::as_str).collect();
            sorted_flags.sort_unstable();
            for flag in sorted_flags {
                flag_pairs.push(format!("{cmd} {flag}"));
            }
        }
        flag_pairs.sort_unstable();
        parts.push(format!("Denied flags: {}", flag_pairs.join(", ")));
    }

    parts
}

/// Format the denial response for a denied shell command.
///
/// The sole deny renderer. Names the denied command, adds the cwd-resolved
/// build `Hint` (when the command has build guidance), and points the agent at
/// `catenary commands` for the full allow / pipeline / deny surface — the
/// surface itself is never dumped inline (see [`format_command_surface`]). The
/// message is identical regardless of how many denials have occurred.
/// `build_hint` is a pre-resolved build guidance string from the caller (when
/// available).
#[must_use]
pub fn format_denial(
    denied_cmd: &str,
    commands: &ResolvedCommands,
    denial: &Denial,
    format: Option<super::HostFormat>,
    build_hint: Option<&str>,
) -> String {
    // Output-redirection denial: a fixed message pointing at the edit tool,
    // independent of the command name, its guidance entry, and the build hint.
    if denial.reason == DenialReason::OutputRedirect {
        return format_redirect_denial(format);
    }

    // Guidance hint (static, build-resolved, or redirect).
    // For the full dump, the base command name is used for lookup (strip
    // subcommand part: "git grep" → "git" won't match, but "grep" will).
    let lookup_cmd = denied_cmd.split_whitespace().next().unwrap_or(denied_cmd);

    // Redirect denial: short format with the command's `-h` output.
    if let Some(crate::config::GuidanceEntry::Redirect { command }) =
        commands.guidance_for(lookup_cmd)
    {
        let opening = format!(
            "`{denied_cmd}` isn't allowed. Use `catenary {command}` instead. Works on any path (LSP enrichment only within tracked roots)."
        );
        let help = render_subcommand_help(command);
        return if help.is_empty() {
            opening
        } else {
            format!("{opening}\n\n{help}")
        };
    }

    let mut parts = vec![format_opening_line(denied_cmd, denial.reason)];

    if let Some(entry) = commands.guidance_for(lookup_cmd) {
        match entry {
            crate::config::GuidanceEntry::Static(msg) => {
                let resolved = resolve_client_vars(msg, format);
                parts.push(format!("Hint: {resolved}"));
            }
            crate::config::GuidanceEntry::Build(_) => {
                // Use caller-provided build hint (full cwd-resolved context).
                if let Some(hint) = build_hint
                    && !hint.is_empty()
                {
                    parts.push(format!("Hint: {hint}"));
                }
            }
            crate::config::GuidanceEntry::Redirect { .. } => {
                // Handled above (early return).
            }
        }
    }

    // The allow / pipeline / deny surface lives behind `catenary commands` now
    // (a pointer, not an inline dump); per-cwd build context already rides the
    // `Hint` line above. See `format_command_surface`.
    parts.push(SURFACE_POINTER.to_string());

    if denial.unresolved_cd {
        parts.push(
            "Note: a `cd` target in this command could not be resolved (variable or \
             command substitution). The build tool check used the original working \
             directory. If the destination has a `.catenary.toml` with a configured \
             build command, run `cd` as a separate command first."
                .to_string(),
        );
    }

    parts.join("\n")
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::literal_string_with_formatting_args,
    reason = "tests use expect for readable assertions; awk/sed program strings contain brace literals that are not format args"
)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use super::*;

    /// Build a rule set matching the Python script's behavior for regression tests.
    ///
    /// The Python script used an allowlist model — this recreates those rules
    /// using the new `ResolvedCommands` allowlist structure.
    fn python_equivalent_rules() -> ResolvedCommands {
        ResolvedCommands {
            allow: HashSet::from([
                "make".into(),
                "git".into(),
                "gh".into(),
                "cp".into(),
                "mv".into(),
                "rm".into(),
                "mkdir".into(),
                "touch".into(),
                "chmod".into(),
                "sleep".into(),
                "cd".into(),
                "true".into(),
                "false".into(),
                "which".into(),
                "diff".into(),
            ]),
            pipeline: HashSet::from([
                "grep".into(),
                "egrep".into(),
                "fgrep".into(),
                "head".into(),
                "tail".into(),
                "sed".into(),
                "awk".into(),
                "sort".into(),
                "jq".into(),
                "wc".into(),
                "tr".into(),
                "cut".into(),
                "uniq".into(),
                "tee".into(),
            ]),
            deny: HashMap::from([(
                "git".into(),
                HashSet::from(["grep".into(), "ls-files".into(), "ls-tree".into()]),
            )]),
            default_build: vec!["make".into()],
            client_enforcement_only: false,
            ..ResolvedCommands::default()
        }
    }

    /// Minimal rule set for targeted tests.
    fn basic_rules() -> ResolvedCommands {
        ResolvedCommands {
            allow: HashSet::from([
                "make".into(),
                "git".into(),
                "gh".into(),
                "echo".into(),
                "diff".into(),
            ]),
            pipeline: HashSet::from(["grep".into(), "egrep".into(), "fgrep".into(), "sed".into()]),
            deny: HashMap::from([(
                "git".into(),
                HashSet::from(["grep".into(), "ls-files".into(), "ls-tree".into()]),
            )]),
            default_build: vec!["make".into()],
            client_enforcement_only: false,
            ..ResolvedCommands::default()
        }
    }

    /// Rule set built from the *shipped* recommended config (`catenary
    /// config`). Derived from the template via
    /// [`config_template::test_recommended`](crate::cli::config_template) so
    /// these behavioral tests track the real default — no hand-maintained
    /// mirror to drift (bugs/12). After ticket 02 the template pipeline
    /// excludes `awk`/`sed` and the edit guidance points `sed` at `catenary
    /// sed`. (The `python_equivalent`/`basic` fixtures keep a permissive
    /// pipeline on purpose — they test parser mechanics, not the default.)
    fn recommended_rules() -> ResolvedCommands {
        let mut rules = ResolvedCommands::default();
        rules.merge(&crate::cli::config_template::test_recommended::config());
        rules
    }

    // ── awk/sed dropped from the recommended pipeline (bugs/12) ───────

    #[test]
    fn awk_denied_any_pipe_position() {
        // awk's program string is quote-masked, hiding system()/print> — so
        // it is out of the pipeline and denied at every position, including
        // mid-pipeline where any allowed command + a pipe would supply the
        // prefix (`true | awk ...`).
        let rules = recommended_rules();
        assert!(
            check_command("awk 'BEGIN{system(\"x\")}'", &rules, None).is_some(),
            "awk denied at position 0",
        );
        assert!(
            check_command("true | awk 'BEGIN{system(\"x\")}'", &rules, None).is_some(),
            "awk denied mid-pipeline",
        );
    }

    #[test]
    fn sed_denied_mid_pipeline() {
        // sed's `w`/GNU `e` and `-i` write/exec from the program string; with
        // sed out of the pipeline, the mid-pipeline allowance no longer lets
        // `… | sed 'w file'` through.
        let rules = recommended_rules();
        assert!(check_command("git log | sed -n 'w /tmp/x'", &rules, None).is_some());
    }

    #[test]
    fn sed_deny_guidance_names_catenary_sed() {
        let rules = recommended_rules();
        let denial = check_command("git log | sed -n 'w /tmp/x'", &rules, None)
            .expect("sed denied mid-pipeline");
        let msg = format_denial(&denial.command, &rules, &denial, None, None);
        assert!(msg.contains("Edit"), "names the edit tool: {msg}");
        assert!(
            msg.contains("catenary sed"),
            "points at catenary sed: {msg}"
        );
    }

    #[test]
    fn filters_still_allowed() {
        // Pure filters that cannot exec/write stay in the pipeline.
        let rules = recommended_rules();
        assert!(check_command("git log | cut -d: -f1", &rules, None).is_none());
        assert!(check_command("git log | sort", &rules, None).is_none());
        assert!(check_command("git log | jq .", &rules, None).is_none());
    }

    // ── reads moved to `allow` (Decision 7, drop read-blocking) ───────

    #[test]
    fn cat_allowed_no_redirect() {
        // `cat` reads a file to stdout — no write vector, so it is allowed.
        let rules = recommended_rules();
        assert!(check_command("cat src/main.rs", &rules, None).is_none());
    }

    #[test]
    fn cat_redirect_still_denied() {
        // Read is fine; the *redirect* is the write vector caught by ticket 01,
        // not by blocking `cat` itself.
        let rules = recommended_rules();
        let denial = check_command("cat foo > bar.rs", &rules, None).expect("redirect denied");
        assert_eq!(denial.reason, DenialReason::OutputRedirect);
    }

    #[test]
    fn reads_allowed_at_position_zero() {
        // The former `guidance.read` set (cat/head/tail/less/more) plus diff
        // now live in `allow`, so they pass at pipeline position 0.
        let rules = recommended_rules();
        for cmd in [
            "head -20 src/main.rs",
            "tail -5 Cargo.toml",
            "less README.md",
            "more README.md",
            "diff a.rs b.rs",
        ] {
            assert!(
                check_command(cmd, &rules, None).is_none(),
                "{cmd} should be allowed",
            );
        }
    }

    #[test]
    fn echo_printf_seq_allowed() {
        // stdout-only generators are allowed; their redirect forms are denied.
        let rules = recommended_rules();
        for cmd in ["echo hello", "printf '%s' x", "seq 1 5"] {
            assert!(check_command(cmd, &rules, None).is_none(), "{cmd} allowed");
        }
        for cmd in ["echo hello > f.txt", "printf x > f.txt", "seq 1 5 > f.txt"] {
            let denial = check_command(cmd, &rules, None).expect("redirect denied");
            assert_eq!(
                denial.reason,
                DenialReason::OutputRedirect,
                "{cmd} redirect denied",
            );
        }
    }

    #[test]
    fn guidance_read_removed() {
        // No "Use {READ} instead" message path remains: reads are allowed, so
        // they carry no guidance, and no static guidance references {READ}.
        let rules = recommended_rules();
        assert!(rules.guidance_for("cat").is_none(), "cat has no guidance");
        assert!(rules.guidance_for("less").is_none(), "less has no guidance");
        assert!(rules.guidance_for("more").is_none(), "more has no guidance");
        assert!(
            !rules.guidance.values().any(|g| matches!(
                g,
                crate::config::GuidanceEntry::Static(msg) if msg.contains("{READ}")
            )),
            "no static guidance should reference the read tool",
        );
    }

    #[test]
    fn scan_list_nudges_kept() {
        // Only the read group is removed — the scan (grep → catenary grep) and
        // list (ls/find → catenary glob) enrichment nudges survive.
        let rules = recommended_rules();

        let grep =
            check_command("grep pattern src", &rules, None).expect("grep denied at position 0");
        let grep_msg = format_denial(&grep.command, &rules, &grep, None, None);
        assert!(
            grep_msg.contains("catenary grep"),
            "grep nudges to catenary grep: {grep_msg}",
        );

        for cmd in ["ls", "find . -name x"] {
            let denial = check_command(cmd, &rules, None).expect("scan/list command denied");
            let msg = format_denial(&denial.command, &rules, &denial, None, None);
            assert!(
                msg.contains("catenary glob"),
                "{cmd} nudges to catenary glob: {msg}",
            );
        }
    }

    // ── Deny basics ──────────────────────────────────────────────────

    #[test]
    fn deny_command_returns_name() {
        let rules = basic_rules();
        let result = check_command("cat file.txt", &rules, None);
        assert_eq!(result.as_ref().map(|d| d.command.as_str()), Some("cat"));
    }

    #[test]
    fn allowed_command_returns_none() {
        let rules = basic_rules();
        assert!(check_command("make check", &rules, None).is_none());
    }

    #[test]
    fn pipeline_at_position_zero_denied() {
        let rules = basic_rules();
        assert!(check_command("grep pattern file", &rules, None).is_some());
    }

    #[test]
    fn pipeline_mid_pipeline_allowed() {
        let rules = basic_rules();
        assert!(check_command("echo foo | grep bar", &rules, None).is_none());
    }

    // ── Pipeline-safe ────────────────────────────────────────────────

    #[test]
    fn grep_standalone_denied() {
        let rules = basic_rules();
        assert!(check_command("grep pattern file", &rules, None).is_some());
    }

    #[test]
    fn grep_mid_pipeline_allowed() {
        let rules = basic_rules();
        assert!(check_command("echo foo | grep bar", &rules, None).is_none());
    }

    #[test]
    fn multi_stage_pipeline_allowed() {
        let rules = python_equivalent_rules();
        assert!(check_command("git log | sort", &rules, None).is_none());
    }

    #[test]
    fn denied_source_blocks_pipeline() {
        let rules = basic_rules();
        assert!(check_command("cat file | grep foo", &rules, None).is_some());
        assert!(check_command("ls | grep foo", &rules, None).is_some());
    }

    // ── Heredoc exception ────────────────────────────────────────────

    #[test]
    fn cat_heredoc_allowed() {
        let rules = basic_rules();
        assert!(check_command("cat <<EOF\nhello\nEOF", &rules, None).is_none());
    }

    #[test]
    fn cat_file_denied() {
        let rules = basic_rules();
        assert!(check_command("cat file.txt", &rules, None).is_some());
    }

    #[test]
    fn head_heredoc_quoted_marker_allowed() {
        // head is not in allow, but heredoc exception applies.
        let mut rules = ResolvedCommands::default();
        rules.allow.insert("git".to_string());
        assert!(check_command("head <<'MARKER'\nhello\nMARKER", &rules, None).is_none());
    }

    #[test]
    fn sed_heredoc_allowed() {
        let rules = basic_rules();
        assert!(check_command("sed 's/foo/bar/' <<EOF\nhello\nEOF", &rules, None).is_none());
    }

    #[test]
    fn heredoc_narrowing_unquoted_arg_before_heredoc() {
        // grep has an unquoted positional arg before <<, so the heredoc
        // exception does NOT fire. grep is in pipeline → denied at pos 0.
        let rules = basic_rules();
        assert!(check_command("grep pattern <<EOF\nhello\nEOF", &rules, None).is_some());
    }

    #[test]
    fn heredoc_narrowing_file_arg_before_heredoc() {
        // Adversarial: file operand before << prevents the exception.
        let rules = basic_rules();
        assert!(check_command("cat file.txt <<EOF\nhello\nEOF", &rules, None).is_some());
    }

    // ── Subshell recursion ───────────────────────────────────────────

    #[test]
    fn subshell_cat_denied() {
        let rules = basic_rules();
        assert!(check_command("echo $(cat file)", &rules, None).is_some());
    }

    #[test]
    fn subshell_grep_in_sequential_denied() {
        let rules = basic_rules();
        assert!(check_command("make test && $(grep -r pattern .)", &rules, None).is_some());
    }

    #[test]
    fn backtick_cat_denied() {
        let rules = basic_rules();
        assert!(check_command("`cat file`", &rules, None).is_some());
    }

    #[test]
    fn process_substitution_cat_denied() {
        let rules = basic_rules();
        assert!(check_command("diff <(cat file1) <(cat file2)", &rules, None).is_some());
    }

    // ── Quote-aware splitting ────────────────────────────────────────

    #[test]
    fn awk_pattern_not_split_on_and() {
        let rules = python_equivalent_rules();
        assert!(check_command("make test | awk '/a/ && /b/' | sort", &rules, None).is_none());
    }

    #[test]
    fn git_commit_message_not_split_on_semicolon() {
        let rules = basic_rules();
        assert!(check_command("git commit -m \"foo; bar\"", &rules, None).is_none());
    }

    #[test]
    fn git_commit_message_not_split_on_and() {
        let rules = basic_rules();
        assert!(check_command("git commit -m \"foo && bar\"", &rules, None).is_none());
    }

    #[test]
    fn pipe_inside_single_quotes_not_split() {
        let rules = python_equivalent_rules();
        assert!(check_command("make test | awk '/a|b/ {print}'", &rules, None).is_none());
    }

    // ── Subcommand deny ──────────────────────────────────────────────

    #[test]
    fn git_grep_denied() {
        let rules = basic_rules();
        assert!(check_command("git grep pattern", &rules, None).is_some());
    }

    #[test]
    fn git_commit_allowed() {
        let rules = basic_rules();
        assert!(check_command("git commit -m \"message\"", &rules, None).is_none());
    }

    #[test]
    fn git_ls_files_denied() {
        let rules = basic_rules();
        assert!(check_command("git ls-files", &rules, None).is_some());
    }

    #[test]
    fn cargo_not_allowed() {
        let rules = basic_rules();
        assert!(check_command("cargo test", &rules, None).is_some());
        assert!(check_command("cargo clippy", &rules, None).is_some());
    }

    // ── Env var prefix ───────────────────────────────────────────────

    #[test]
    fn env_var_prefix_allowed() {
        let rules = basic_rules();
        assert!(check_command("DEBUG=1 make test", &rules, None).is_none());
    }

    #[test]
    fn env_var_prefix_denied() {
        let rules = basic_rules();
        assert!(check_command("RUST_LOG=debug cargo test", &rules, None).is_some());
    }

    #[test]
    fn multiple_env_vars_denied() {
        let rules = basic_rules();
        assert!(check_command("A=1 B=2 cat file", &rules, None).is_some());
    }

    // ── Full path ────────────────────────────────────────────────────

    #[test]
    fn full_path_grep_denied() {
        let rules = basic_rules();
        assert!(check_command("/usr/bin/grep pattern", &rules, None).is_some());
    }

    #[test]
    fn full_path_cat_denied() {
        let rules = basic_rules();
        assert!(check_command("/bin/cat file.txt", &rules, None).is_some());
    }

    #[test]
    fn relative_path_denied() {
        let rules = basic_rules();
        assert!(check_command("./grep foo bar", &rules, None).is_some());
        assert!(check_command("../bin/grep foo bar", &rules, None).is_some());
    }

    // ── Regression tests (ported from Python) ────────────────────────

    mod regression {
        use super::*;

        // TestAllowed
        #[test]
        fn make() {
            let rules = python_equivalent_rules();
            assert!(check_command("make check", &rules, None).is_none());
        }

        #[test]
        fn git() {
            let rules = python_equivalent_rules();
            assert!(check_command("git status", &rules, None).is_none());
            assert!(check_command("git log --oneline", &rules, None).is_none());
            assert!(check_command("git commit -m 'fix bug'", &rules, None).is_none());
        }

        #[test]
        fn gh() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list", &rules, None).is_none());
            assert!(check_command("gh issue view 123", &rules, None).is_none());
        }

        #[test]
        fn sleep() {
            let rules = python_equivalent_rules();
            assert!(check_command("sleep 5", &rules, None).is_none());
        }

        #[test]
        fn cp_mv() {
            let rules = python_equivalent_rules();
            assert!(check_command("cp foo bar", &rules, None).is_none());
            assert!(check_command("mv foo bar", &rules, None).is_none());
        }

        #[test]
        fn env_prefix_allowed() {
            let rules = python_equivalent_rules();
            assert!(check_command("DEBUG=1 make check", &rules, None).is_none());
            assert!(check_command("RUST_LOG=debug make test", &rules, None).is_none());
        }

        // TestDenied
        #[test]
        fn cat_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("cat file.txt", &rules, None).is_some());
        }

        #[test]
        fn grep_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("grep foo bar.rs", &rules, None).is_some());
        }

        #[test]
        fn ls_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("ls -la", &rules, None).is_some());
        }

        #[test]
        fn find_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("find . -name '*.rs'", &rules, None).is_some());
        }

        #[test]
        fn cargo_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("cargo build", &rules, None).is_some());
            assert!(check_command("cargo test", &rules, None).is_some());
            assert!(check_command("cargo build 2>&1", &rules, None).is_some());
        }

        #[test]
        fn full_path_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("/usr/bin/grep foo bar", &rules, None).is_some());
            assert!(check_command("/bin/cat file.txt", &rules, None).is_some());
        }

        #[test]
        fn env_prefix_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("DEBUG=1 cargo test", &rules, None).is_some());
        }

        // TestGitDeniedSubcommands
        #[test]
        fn git_grep_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git grep foo", &rules, None).is_some());
        }

        #[test]
        fn git_ls_files_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git ls-files", &rules, None).is_some());
        }

        #[test]
        fn git_ls_tree_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git ls-tree HEAD", &rules, None).is_some());
        }

        #[test]
        fn git_log_allowed() {
            let rules = python_equivalent_rules();
            assert!(check_command("git log --oneline", &rules, None).is_none());
        }

        #[test]
        fn git_diff_allowed() {
            let rules = python_equivalent_rules();
            assert!(check_command("git diff HEAD", &rules, None).is_none());
        }

        // TestPipeline
        #[test]
        fn grep_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list | grep foo", &rules, None).is_none());
        }

        #[test]
        fn head_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh issue list | head -20", &rules, None).is_none());
        }

        #[test]
        fn tail_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("git log --oneline | tail -5", &rules, None).is_none());
        }

        #[test]
        fn jq_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr view --json title | jq .title", &rules, None).is_none());
        }

        #[test]
        fn wc_mid_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh issue list | wc -l", &rules, None).is_none());
        }

        #[test]
        fn multi_stage_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list | grep open | head -5", &rules, None).is_none());
        }

        #[test]
        fn grep_standalone_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("grep foo bar.rs", &rules, None).is_some());
        }

        #[test]
        fn denied_source_blocks_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("cat file | grep foo", &rules, None).is_some());
            assert!(check_command("ls | grep foo", &rules, None).is_some());
        }

        // TestHeredoc
        #[test]
        fn git_commit_heredoc() {
            let rules = python_equivalent_rules();
            assert!(
                check_command(
                    "git commit -m \"$(cat <<'EOF'\nmessage\nEOF\n)\"",
                    &rules,
                    None
                )
                .is_none()
            );
        }

        #[test]
        fn gh_pr_create_heredoc() {
            let rules = python_equivalent_rules();
            assert!(
                check_command(
                    "gh pr create --body \"$(cat <<'EOF'\nbody text\nEOF\n)\"",
                    &rules,
                    None
                )
                .is_none()
            );
        }

        #[test]
        fn heredoc_body_with_semicolons() {
            let rules = python_equivalent_rules();
            let cmd = "git commit -m \"$(cat <<'EOF'\n\
                        feat: fix hook deny response\n\
                        \n\
                        - Fix display; add suppressOutput and systemMessage\n\
                        - Add chmod +x to script (missing execute bit)\n\
                        EOF\n\
                        )\"";
            assert!(check_command(cmd, &rules, None).is_none());
        }

        #[test]
        fn heredoc_body_with_parentheses() {
            let rules = python_equivalent_rules();
            let cmd = "git commit -m \"$(cat <<'EOF'\n\
                        fix(hook): missing execute bit was silently allowing blocked commands through)\n\
                        EOF\n\
                        )\"";
            assert!(check_command(cmd, &rules, None).is_none());
        }

        #[test]
        fn cat_file_still_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("cat file.txt", &rules, None).is_some());
        }

        // TestSubshell
        #[test]
        fn subshell_cat_standalone() {
            let rules = python_equivalent_rules();
            assert!(check_command("$(cat Makefile)", &rules, None).is_some());
        }

        #[test]
        fn subshell_cat_in_git_arg() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"$(cat file)\"", &rules, None).is_some());
        }

        #[test]
        fn backtick_grep() {
            let rules = python_equivalent_rules();
            assert!(check_command("`grep foo bar`", &rules, None).is_some());
        }

        #[test]
        fn backtick_cat() {
            let rules = python_equivalent_rules();
            assert!(check_command("make build `cat args.txt`", &rules, None).is_some());
        }

        // TestProcessSubstitution
        #[test]
        fn cat_inside_process_sub_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git diff <(cat file1) <(cat file2)", &rules, None).is_some());
        }

        #[test]
        fn grep_inside_process_sub_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git diff <(grep foo bar)", &rules, None).is_some());
        }

        #[test]
        fn git_show_inside_process_sub_allowed() {
            let rules = python_equivalent_rules();
            assert!(
                check_command(
                    "git diff <(git show HEAD:src/main.rs) <(git show HEAD~1:src/main.rs)",
                    &rules,
                    None
                )
                .is_none()
            );
        }

        // TestSequential
        #[test]
        fn and_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("make build && cat file", &rules, None).is_some());
        }

        #[test]
        fn semicolon_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git status; ls", &rules, None).is_some());
        }

        #[test]
        fn or_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("make check || cargo test", &rules, None).is_some());
        }

        #[test]
        fn both_allowed() {
            let rules = python_equivalent_rules();
            assert!(check_command("git fetch && make check", &rules, None).is_none());
        }

        // TestAdversarial
        #[test]
        fn env_var_before_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("FOO=0 grep foo bar", &rules, None).is_some());
        }

        #[test]
        fn multiple_env_vars_before_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("A=1 B=2 cat file", &rules, None).is_some());
        }

        #[test]
        fn env_var_before_denied_full_path() {
            let rules = python_equivalent_rules();
            assert!(check_command("PATH=/tmp /usr/bin/grep foo bar", &rules, None).is_some());
        }

        #[test]
        fn subshell_with_internal_spaces() {
            let rules = python_equivalent_rules();
            assert!(check_command("$( cat file )", &rules, None).is_some());
        }

        #[test]
        fn nested_subshell() {
            let rules = python_equivalent_rules();
            assert!(check_command("$(echo $(cat file))", &rules, None).is_some());
        }

        #[test]
        fn subshell_in_pipeline_position() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list | $(cat file)", &rules, None).is_some());
        }

        #[test]
        fn subshell_grep_in_pipeline() {
            let rules = python_equivalent_rules();
            assert!(check_command("gh pr list | $(grep foo bar.rs)", &rules, None).is_some());
        }

        #[test]
        fn backtick_in_git_arg() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"`cat file`\"", &rules, None).is_some());
        }

        #[test]
        fn semicolon_leading_subshell() {
            let rules = python_equivalent_rules();
            assert!(check_command("; $(head file)", &rules, None).is_some());
        }

        #[test]
        fn semicolon_then_cat_subshell() {
            let rules = python_equivalent_rules();
            assert!(check_command("make check; $(cat Makefile)", &rules, None).is_some());
        }

        #[test]
        fn semicolon_then_grep() {
            let rules = python_equivalent_rules();
            assert!(check_command("git status; grep foo bar", &rules, None).is_some());
        }

        #[test]
        fn herestring_with_subshell_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("head -5 <<< $(cat /etc/passwd)", &rules, None).is_some());
        }

        #[test]
        fn logical_or_both_checked() {
            let rules = python_equivalent_rules();
            assert!(check_command("make check || grep foo bar", &rules, None).is_some());
        }

        #[test]
        fn relative_path_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("./grep foo bar", &rules, None).is_some());
            assert!(check_command("../bin/grep foo bar", &rules, None).is_some());
        }

        #[test]
        fn git_diff_process_substitution_denied() {
            let rules = python_equivalent_rules();
            assert!(check_command("git diff <(cat file1) <(cat file2)", &rules, None).is_some());
        }

        // TestQuotedOperators
        #[test]
        fn awk_with_and_in_pattern() {
            let rules = python_equivalent_rules();
            assert!(check_command("make test | awk '/a/ && /b/' | sort", &rules, None).is_none());
        }

        #[test]
        fn pipe_inside_single_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("make test | awk '/a|b/ {print}'", &rules, None).is_none());
        }

        #[test]
        fn semicolon_inside_single_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("make ARGS='a;b;c' test", &rules, None).is_none());
        }

        #[test]
        fn and_inside_double_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"foo && bar\"", &rules, None).is_none());
        }

        #[test]
        fn pipe_inside_double_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"a | b\"", &rules, None).is_none());
        }

        #[test]
        fn semicolon_inside_double_quotes() {
            let rules = python_equivalent_rules();
            assert!(check_command("git commit -m \"a; b\"", &rules, None).is_none());
        }

        #[test]
        fn unquoted_operators_still_split() {
            let rules = python_equivalent_rules();
            assert!(check_command("make build && cat file", &rules, None).is_some());
            assert!(check_command("make build; ls .", &rules, None).is_some());
            assert!(check_command("cat file | grep foo", &rules, None).is_some());
        }
    }

    // ── mask_quotes unit tests ───────────────────────────────────────

    #[test]
    fn mask_quotes_single() {
        let result = mask_quotes("echo 'foo && bar'");
        assert!(!result.contains("foo"));
        assert_eq!(result.len(), "echo 'foo && bar'".len());
    }

    #[test]
    fn mask_quotes_double() {
        let result = mask_quotes("echo \"foo | bar\"");
        assert!(!result.contains("foo"));
        assert_eq!(result.len(), "echo \"foo | bar\"".len());
    }

    #[test]
    fn mask_quotes_preserves_unquoted() {
        let result = mask_quotes("echo hello && world");
        assert!(result.contains("echo"));
        assert!(result.contains("hello"));
        assert!(result.contains("&&"));
        assert!(result.contains("world"));
    }

    // ── strip_heredoc_bodies tests ───────────────────────────────────

    #[test]
    fn strip_heredoc_removes_body() {
        let input = "cat <<EOF\nhello world\nfoo bar\nEOF";
        let result = strip_heredoc_bodies(input);
        assert!(!result.contains("hello world"));
        assert!(!result.contains("foo bar"));
        assert!(result.contains("cat <<EOF"));
        assert!(result.contains("EOF"));
    }

    #[test]
    fn strip_heredoc_preserves_non_heredoc() {
        let input = "make build && git status";
        let result = strip_heredoc_bodies(input);
        assert_eq!(result, input);
    }

    // ── find_command tests ───────────────────────────────────────────

    #[test]
    fn find_command_no_env_vars() {
        assert_eq!(find_command(&["make", "test"]), Some(0));
    }

    #[test]
    fn find_command_skips_env_vars() {
        assert_eq!(find_command(&["DEBUG=1", "make", "test"]), Some(1));
        assert_eq!(find_command(&["A=1", "B=2", "cat", "file"]), Some(2));
    }

    #[test]
    fn find_command_all_env_vars() {
        assert_eq!(find_command(&["A=1", "B=2"]), None);
    }

    // ── pipe_split tests ─────────────────────────────────────────────

    #[test]
    fn pipe_split_basic() {
        let parts = pipe_split("echo foo | grep bar");
        assert_eq!(parts, vec!["echo foo", "grep bar"]);
    }

    #[test]
    fn pipe_split_preserves_or() {
        let parts = pipe_split("make check || cargo test");
        assert_eq!(parts, vec!["make check || cargo test"]);
    }

    #[test]
    fn pipe_split_multi_stage() {
        let parts = pipe_split("a | b | c");
        assert_eq!(parts, vec!["a", "b", "c"]);
    }

    #[test]
    fn pipe_split_quoted_pipe() {
        let parts = pipe_split("git commit -m \"a | b\"");
        assert_eq!(parts, vec!["git commit -m \"a | b\""]);
    }

    // ── extract_command_names tests ─────────────────────────────────

    #[test]
    fn extract_names_simple() {
        let names = extract_command_names("rm -rf target/");
        assert_eq!(names, vec!["rm"]);
    }

    #[test]
    fn extract_names_chained() {
        let names = extract_command_names("mkdir -p src/new && touch src/new/mod.rs");
        assert_eq!(names, vec!["mkdir", "touch"]);
    }

    #[test]
    fn extract_names_pipeline() {
        let names = extract_command_names("find . -name '*.rs' | grep test");
        assert_eq!(names, vec!["find", "grep"]);
    }

    #[test]
    fn extract_names_full_path() {
        let names = extract_command_names("/usr/bin/cp a b");
        assert_eq!(names, vec!["cp"]);
    }

    #[test]
    fn extract_names_env_prefix() {
        let names = extract_command_names("LANG=C rm foo.rs");
        assert_eq!(names, vec!["rm"]);
    }

    #[test]
    fn extract_names_subshell() {
        // The faithful parse projects the command word before its recursed
        // substitutions (`rm` then `cat`); the consumer treats this as a set,
        // so the order is not significant.
        let names = extract_command_names("rm $(cat files.txt)");
        assert_eq!(names, vec!["rm", "cat"]);
    }

    #[test]
    fn extract_names_empty() {
        let names = extract_command_names("");
        assert!(names.is_empty());
    }

    // ── Denial format tests ────────────────────────────────────────────

    fn no_cd_denial(cmd: &str) -> Denial {
        Denial {
            effective_cwd: None,
            command: cmd.to_string(),
            reason: DenialReason::NotAllowed,
            unresolved_cd: false,
        }
    }

    #[test]
    fn format_denial_includes_commands_pointer() {
        let rules = python_equivalent_rules();
        let msg = format_denial("ls", &rules, &no_cd_denial("ls"), None, None);

        assert!(msg.starts_with("`ls` isn't allowed"), "opening line: {msg}");
        assert!(
            msg.contains("Run `catenary commands` for the allowed command surface."),
            "deny renderer should point at `catenary commands`: {msg}",
        );
        // The surface itself moved behind `catenary commands` — no inline dump.
        assert!(!msg.contains("Allowed:"), "no inline allow section: {msg}");
        assert!(
            !msg.contains("Allowed in pipelines"),
            "no inline pipeline section: {msg}",
        );
        assert!(
            !msg.contains("Denied subcommands"),
            "no inline deny section: {msg}",
        );
    }

    #[test]
    fn format_full_omits_build_dump() {
        let rules = ResolvedCommands {
            allow: HashSet::from(["git".into()]),
            default_build: vec!["make".into(), "npm".into()],
            build: HashMap::from([(
                std::path::PathBuf::from("/project"),
                vec!["cargo".into(), "npm".into()],
            )]),
            ..ResolvedCommands::default()
        };
        let msg = format_denial("ls", &rules, &no_cd_denial("ls"), None, None);
        // Per-root / default build tools are no longer dumped — the cwd build
        // context rides the `Hint` line (build-command denials only).
        assert!(
            !msg.contains("Default build tool"),
            "no default build line: {msg}",
        );
        assert!(
            !msg.contains("Build tool for"),
            "no per-root build line: {msg}",
        );
    }

    #[test]
    fn surface_all_sections() {
        let rules = python_equivalent_rules();
        let surface = format_command_surface(&rules).join("\n");

        assert!(surface.contains("Allowed:"), "allow section: {surface}");
        assert!(
            surface.contains("Allowed in pipelines (not first):"),
            "pipeline section: {surface}",
        );
        assert!(
            surface.contains("Denied subcommands:"),
            "deny section: {surface}",
        );
    }

    #[test]
    fn surface_sorted_alphabetically() {
        let rules = python_equivalent_rules();
        let surface = format_command_surface(&rules).join("\n");

        let allowed_line = surface
            .lines()
            .find(|l| l.starts_with("Allowed:"))
            .expect("Allowed line");
        let items: Vec<&str> = allowed_line
            .strip_prefix("Allowed: ")
            .expect("prefix")
            .split(", ")
            .collect();
        let mut sorted = items.clone();
        sorted.sort_unstable();
        assert_eq!(items, sorted, "allow list should be sorted");
    }

    #[test]
    fn surface_omits_empty_sections() {
        let rules = ResolvedCommands {
            allow: HashSet::from(["git".into()]),
            ..ResolvedCommands::default()
        };
        let surface = format_command_surface(&rules).join("\n");

        assert!(surface.contains("Allowed: git"));
        assert!(
            !surface.contains("Allowed in pipelines"),
            "empty pipeline should be omitted"
        );
        assert!(
            !surface.contains("Denied subcommands"),
            "empty deny should be omitted"
        );
        assert!(
            !surface.contains("Denied flags"),
            "empty deny_flags should be omitted"
        );
    }

    #[test]
    fn surface_deny_pairs_sorted() {
        let rules = ResolvedCommands {
            allow: HashSet::from(["git".into(), "sqlite3".into()]),
            deny: HashMap::from([
                (
                    "git".into(),
                    HashSet::from(["ls-files".into(), "grep".into(), "ls-tree".into()]),
                ),
                ("sqlite3".into(), HashSet::from(["-cmd".into()])),
            ]),
            ..ResolvedCommands::default()
        };
        let surface = format_command_surface(&rules).join("\n");

        let deny_line = surface
            .lines()
            .find(|l| l.starts_with("Denied subcommands:"))
            .expect("deny line");
        let items: Vec<&str> = deny_line
            .strip_prefix("Denied subcommands: ")
            .expect("prefix")
            .split(", ")
            .collect();
        let mut sorted = items.clone();
        sorted.sort_unstable();
        assert_eq!(items, sorted, "deny pairs should be sorted");
    }

    #[test]
    fn check_command_denied_subcommand_returns_full_form() {
        let rules = python_equivalent_rules();
        // git grep should return "git grep", not just "git".
        let denied = check_command("git grep foo", &rules, None);
        assert_eq!(
            denied.as_ref().map(|d| d.command.as_str()),
            Some("git grep"),
        );
    }

    // ── cd resolution tests ──────────────────────────────────────────

    /// Rules with per-root build tools for cd resolution tests.
    fn cd_rules() -> ResolvedCommands {
        ResolvedCommands {
            allow: HashSet::from(["git".into(), "cd".into()]),
            build: HashMap::from([
                (std::path::PathBuf::from("/project/a"), vec!["make".into()]),
                (std::path::PathBuf::from("/project/b"), vec!["npm".into()]),
            ]),
            ..ResolvedCommands::default()
        }
    }

    #[test]
    fn cd_absolute_updates_effective_cwd() {
        let rules = cd_rules();
        // npm is the build tool for /project/b — allowed after cd.
        assert!(
            check_command(
                "cd /project/b && npm install",
                &rules,
                Some(std::path::Path::new("/project/a"))
            )
            .is_none(),
            "npm should be allowed after cd to /project/b",
        );
    }

    #[test]
    fn cd_absolute_denies_wrong_build() {
        let rules = cd_rules();
        // make is NOT the build tool for /project/b.
        assert_eq!(
            check_command(
                "cd /project/b && make check",
                &rules,
                Some(std::path::Path::new("/project/a"))
            )
            .as_ref()
            .map(|d| d.command.as_str()),
            Some("make"),
        );
    }

    #[test]
    fn cd_relative_resolves_against_cwd() {
        let rules = cd_rules();
        // Starting at /project, cd b → /project/b, npm is build tool there.
        assert!(
            check_command(
                "cd b && npm install",
                &rules,
                Some(std::path::Path::new("/project"))
            )
            .is_none(),
        );
    }

    #[test]
    fn cd_tilde_expands_home() {
        // Just verify resolve_cd_target handles ~ correctly.
        let result = resolve_cd_target("~/projects", Some(std::path::Path::new("/tmp")));
        let home = dirs::home_dir().expect("HOME");
        assert_eq!(result, Some(home.join("projects")));
    }

    #[test]
    fn cd_variable_preserves_cwd() {
        // Can't resolve $VAR — effective cwd stays unchanged.
        let result = resolve_cd_target("$PROJECT", Some(std::path::Path::new("/original")));
        assert_eq!(result, Some(std::path::PathBuf::from("/original")));
    }

    #[test]
    fn cd_parent_normalized() {
        let rules = cd_rules();
        // cd /project/b/../a → /project/a, make is build tool there.
        assert!(
            check_command(
                "cd /project/b/../a && make check",
                &rules,
                Some(std::path::Path::new("/tmp")),
            )
            .is_none(),
        );
    }

    #[test]
    fn without_cd_uses_original_cwd() {
        let rules = cd_rules();
        // No cd — cwd is /project/a, make is the build tool.
        assert!(
            check_command(
                "make check",
                &rules,
                Some(std::path::Path::new("/project/a"))
            )
            .is_none()
        );
        // npm is NOT the build tool for /project/a.
        assert!(
            check_command(
                "npm install",
                &rules,
                Some(std::path::Path::new("/project/a"))
            )
            .is_some()
        );
    }

    #[test]
    fn cd_unresolved_variable_flags_denial() {
        let rules = cd_rules();
        // cd $PROJECT_DIR can't be resolved — denial should flag it.
        let denial = check_command(
            "cd $PROJECT_DIR && npm install",
            &rules,
            Some(std::path::Path::new("/project/a")),
        )
        .expect("should deny npm");
        assert!(
            denial.unresolved_cd,
            "denial should flag unresolved cd target"
        );
        assert_eq!(denial.command, "npm");
    }

    #[test]
    fn cd_resolved_does_not_flag() {
        let rules = cd_rules();
        // cd /project/b resolves fine — denial (if any) should not flag.
        let denial = check_command(
            "cd /project/b && make check",
            &rules,
            Some(std::path::Path::new("/project/a")),
        )
        .expect("make denied in /project/b");
        assert!(!denial.unresolved_cd, "resolved cd should not flag");
    }

    #[test]
    fn format_full_includes_unresolved_cd_note() {
        let rules = cd_rules();
        let denial = Denial {
            command: "npm".into(),
            reason: DenialReason::NotAllowed,
            unresolved_cd: true,
            effective_cwd: None,
        };
        let msg = format_denial("npm", &rules, &denial, None, None);
        assert!(
            msg.contains("could not be resolved"),
            "should include unresolved cd note: {msg}",
        );
    }

    #[test]
    fn format_full_omits_note_when_resolved() {
        let rules = cd_rules();
        let denial = Denial {
            command: "npm".into(),
            reason: DenialReason::NotAllowed,
            unresolved_cd: false,
            effective_cwd: None,
        };
        let msg = format_denial("npm", &rules, &denial, None, None);
        assert!(
            !msg.contains("could not be resolved"),
            "should not include note when resolved: {msg}",
        );
    }

    // ── Guidance tests ────────────────────────────────────────────────

    fn rules_with_guidance() -> ResolvedCommands {
        use crate::config::{BuildGuidance, GuidanceEntry};

        let mut rules = basic_rules();
        rules.guidance.insert(
            "grep".to_string(),
            GuidanceEntry::Static("Use Catenary's grep tool instead".to_string()),
        );
        rules.guidance.insert(
            "cat".to_string(),
            GuidanceEntry::Static("Use {READ} instead".to_string()),
        );
        rules.guidance.insert(
            "cargo".to_string(),
            GuidanceEntry::Build(BuildGuidance::default()),
        );
        rules
    }

    #[test]
    fn format_full_static_guidance() {
        let rules = rules_with_guidance();
        let denial = Denial {
            command: "grep".to_string(),
            reason: DenialReason::PipelinePosition,
            unresolved_cd: false,
            effective_cwd: None,
        };
        let msg = format_denial("grep", &rules, &denial, None, None);
        assert!(
            msg.contains("Hint: Use Catenary's grep tool instead"),
            "should include guidance hint: {msg}",
        );
    }

    #[test]
    fn format_full_pipeline_opening_line() {
        let rules = rules_with_guidance();
        let denial = Denial {
            command: "grep".to_string(),
            reason: DenialReason::PipelinePosition,
            unresolved_cd: false,
            effective_cwd: None,
        };
        let msg = format_denial("grep", &rules, &denial, None, None);
        assert!(
            msg.starts_with("`grep` isn't allowed at the start of a pipeline."),
            "pipeline opening line: {msg}",
        );
    }

    #[test]
    fn format_full_denied_subcommand_opening_line() {
        let rules = rules_with_guidance();
        let denial = Denial {
            command: "git grep".to_string(),
            reason: DenialReason::DeniedSubcommand,
            unresolved_cd: false,
            effective_cwd: None,
        };
        let msg = format_denial("git grep", &rules, &denial, None, None);
        assert!(
            msg.starts_with("`git grep` isn't allowed (denied subcommand)."),
            "subcommand opening line: {msg}",
        );
    }

    #[test]
    fn format_full_no_guidance_fallback() {
        let rules = basic_rules();
        let denial = no_cd_denial("ls");
        let msg = format_denial("ls", &rules, &denial, None, None);
        assert!(
            !msg.contains("Hint:"),
            "no guidance should mean no Hint line: {msg}",
        );
    }

    #[test]
    fn format_full_read_edit_template_vars_default() {
        let rules = rules_with_guidance();
        let denial = no_cd_denial("cat");
        let msg = format_denial("cat", &rules, &denial, None, None);
        assert!(
            msg.contains("Hint: Use Read instead"),
            "{{READ}} should resolve to Read by default: {msg}",
        );
    }

    #[test]
    fn format_full_read_edit_template_vars_claude() {
        let rules = rules_with_guidance();
        let denial = no_cd_denial("cat");
        let msg = format_denial(
            "cat",
            &rules,
            &denial,
            Some(crate::cli::HostFormat::Claude),
            None,
        );
        assert!(
            msg.contains("Hint: Use Read instead"),
            "{{READ}} should resolve to Read for Claude: {msg}",
        );
    }

    #[test]
    fn format_full_read_edit_template_vars_gemini() {
        let rules = rules_with_guidance();
        let denial = no_cd_denial("cat");
        let msg = format_denial(
            "cat",
            &rules,
            &denial,
            Some(crate::cli::HostFormat::Gemini),
            None,
        );
        assert!(
            msg.contains("Hint: Use read_file instead"),
            "{{READ}} should resolve to read_file for Gemini: {msg}",
        );
    }

    #[test]
    fn format_full_build_guidance_with_hint() {
        let rules = rules_with_guidance();
        let denial = no_cd_denial("cargo");
        let hint = "User config has make as the default build tool.\n\
                     No local `.catenary.toml` was found.";
        let msg = format_denial("cargo", &rules, &denial, None, Some(hint));
        assert!(
            msg.contains("Hint: User config has make"),
            "build guidance should show resolved hint: {msg}"
        );
        assert!(
            msg.contains("No local `.catenary.toml`"),
            "build guidance should show project line: {msg}",
        );
    }

    #[test]
    fn format_full_build_guidance_without_hint() {
        let rules = rules_with_guidance();
        let denial = no_cd_denial("cargo");
        let msg = format_denial("cargo", &rules, &denial, None, None);
        assert!(
            !msg.contains("Hint:"),
            "no build_hint should mean no Hint line: {msg}",
        );
    }

    #[test]
    fn denial_reason_pipeline_position() {
        let rules = basic_rules();
        let denial = check_command("grep pattern file", &rules, None)
            .expect("grep should be denied at pos 0");
        assert_eq!(denial.reason, DenialReason::PipelinePosition);
    }

    #[test]
    fn denial_reason_not_allowed() {
        let rules = basic_rules();
        let denial = check_command("cat file.txt", &rules, None).expect("cat should be denied");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn denial_reason_denied_subcommand() {
        let rules = basic_rules();
        let denial =
            check_command("git grep foo", &rules, None).expect("git grep should be denied");
        assert_eq!(denial.reason, DenialReason::DeniedSubcommand);
    }

    // ── Flag deny tests ─────────────────────────────────────────────

    fn rules_with_deny_flags() -> ResolvedCommands {
        let mut rules = basic_rules();
        rules.deny_flags = HashMap::from([
            ("make".into(), HashSet::from(["-C".into()])),
            ("cargo".into(), HashSet::from(["--manifest-path".into()])),
        ]);
        // Add cargo as a build tool so it's allowed
        rules.default_build = vec!["make".into(), "cargo".into()];
        // cd needed for cd-then-build tests
        rules.allow.insert("cd".into());
        rules
    }

    #[test]
    fn deny_flag_make_c() {
        let rules = rules_with_deny_flags();
        let denial = check_command("make -C dir", &rules, None).expect("make -C denied");
        assert_eq!(denial.reason, DenialReason::DeniedFlag);
        assert_eq!(denial.command, "make -C");
    }

    #[test]
    fn deny_flag_cargo_manifest_path() {
        let rules = rules_with_deny_flags();
        let denial = check_command("cargo build --manifest-path Cargo.toml", &rules, None)
            .expect("cargo --manifest-path denied");
        assert_eq!(denial.reason, DenialReason::DeniedFlag);
        assert_eq!(denial.command, "cargo --manifest-path");
    }

    #[test]
    fn deny_flag_cargo_manifest_path_eq() {
        let rules = rules_with_deny_flags();
        let denial = check_command("cargo --manifest-path=Cargo.toml build", &rules, None)
            .expect("cargo --manifest-path=value denied");
        assert_eq!(denial.reason, DenialReason::DeniedFlag);
        assert_eq!(denial.command, "cargo --manifest-path");
    }

    #[test]
    fn deny_flag_make_without_c_allowed() {
        let rules = rules_with_deny_flags();
        assert!(check_command("make check", &rules, None).is_none());
    }

    #[test]
    fn deny_flag_no_combined_decomposition() {
        // -rf should NOT match -r
        let mut rules = basic_rules();
        rules.deny_flags = HashMap::from([("make".into(), HashSet::from(["-r".into()]))]);
        assert!(
            check_command("make -rf", &rules, None).is_none(),
            "-rf should not match -r",
        );
    }

    #[test]
    fn deny_flag_cd_then_make_allowed() {
        let rules = rules_with_deny_flags();
        // cd dir && make is fine — not affected by deny_flags
        assert!(
            check_command(
                "cd dir && make check",
                &rules,
                Some(std::path::Path::new("/project")),
            )
            .is_none(),
        );
    }

    #[test]
    fn deny_flag_opening_line() {
        let rules = rules_with_deny_flags();
        let denial = Denial {
            command: "make -C".into(),
            reason: DenialReason::DeniedFlag,
            unresolved_cd: false,
            effective_cwd: None,
        };
        let msg = format_denial("make -C", &rules, &denial, None, None);
        assert!(
            msg.starts_with("`make -C` isn't allowed (denied flag)."),
            "flag denial opening: {msg}",
        );
    }

    #[test]
    fn surface_denied_flags_section() {
        let rules = rules_with_deny_flags();
        let surface = format_command_surface(&rules).join("\n");
        assert!(
            surface.contains("Denied flags:"),
            "should have denied flags section: {surface}",
        );
        assert!(
            surface.contains("cargo --manifest-path"),
            "should list cargo --manifest-path: {surface}",
        );
        assert!(
            surface.contains("make -C"),
            "should list make -C: {surface}"
        );
    }

    #[test]
    fn multi_build_tool_both_allowed() {
        let rules = ResolvedCommands {
            allow: HashSet::from(["git".into()]),
            default_build: vec!["make".into(), "npm".into()],
            ..ResolvedCommands::default()
        };
        assert!(check_command("make check", &rules, None).is_none());
        assert!(check_command("npm install", &rules, None).is_none());
    }

    #[test]
    fn multi_build_tool_non_member_denied() {
        let rules = ResolvedCommands {
            allow: HashSet::from(["git".into()]),
            default_build: vec!["make".into(), "npm".into()],
            ..ResolvedCommands::default()
        };
        let denial = check_command("cargo build", &rules, None).expect("cargo should be denied");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }

    // ── Output redirection tests ────────────────────────────────────

    #[test]
    fn redirect_to_file_denied() {
        let rules = basic_rules();
        // git is allowed, but the redirect must still be denied.
        let denial = check_command("git status > out.txt", &rules, None).expect("redirect denied");
        assert_eq!(denial.reason, DenialReason::OutputRedirect);
    }

    #[test]
    fn redirect_append_denied() {
        let rules = basic_rules();
        let denial =
            check_command("git log >> out.txt", &rules, None).expect("append redirect denied");
        assert_eq!(denial.reason, DenialReason::OutputRedirect);
    }

    #[test]
    fn redirect_glued_target_denied() {
        let rules = basic_rules();
        for cmd in ["echo x>file", "make test 2>file", "make test &>file"] {
            let denial = check_command(cmd, &rules, None).expect("glued redirect denied");
            assert_eq!(
                denial.reason,
                DenialReason::OutputRedirect,
                "glued redirect should be OutputRedirect: {cmd}",
            );
        }
    }

    #[test]
    fn redirect_clobber_denied() {
        let rules = basic_rules();
        let denial =
            check_command("git status >| out.txt", &rules, None).expect("clobber redirect denied");
        assert_eq!(denial.reason, DenialReason::OutputRedirect);
    }

    #[test]
    fn heredoc_plus_redirect_denied() {
        // The redirect check runs before the heredoc exception, so the
        // stdin-reading short-circuit can't smuggle a file write through.
        let rules = basic_rules();
        let denial = check_command("cat <<'EOF' > file.rs\nfn main() {}\nEOF", &rules, None)
            .expect("heredoc + redirect denied");
        assert_eq!(denial.reason, DenialReason::OutputRedirect);
    }

    #[test]
    fn fd_dup_allowed() {
        let rules = basic_rules();
        // make is the build tool (allowed); the fd-dups carry no file target.
        assert!(check_command("make test 2>&1", &rules, None).is_none());
        assert!(check_command("make test >&2", &rules, None).is_none());
    }

    #[test]
    fn device_sink_allowed() {
        let rules = basic_rules();
        assert!(check_command("make test > /dev/null", &rules, None).is_none());
        assert!(check_command("make test > /dev/stdout", &rules, None).is_none());
        assert!(check_command("make test > /dev/stderr", &rules, None).is_none());
        // Device sink with a trailing fd-dup is still allowed.
        assert!(check_command("make test > /dev/null 2>&1", &rules, None).is_none());
    }

    #[test]
    fn redirect_inside_quotes_allowed() {
        let rules = basic_rules();
        // The `>` is inside a quoted argument — not a real redirect.
        assert!(check_command("git commit -m \"a > b\"", &rules, None).is_none());
    }

    #[test]
    fn allow_file_redirects_true_permits() {
        let mut rules = basic_rules();
        rules.allow_file_redirects = true;
        // git is allowed and the flag lifts the redirect deny.
        assert!(check_command("git status > out.txt", &rules, None).is_none());
    }

    #[test]
    fn tee_file_operand_handled() {
        // `tee` is absent from the default pipeline, so a `tee <file>` write
        // vector is denied (NotAllowed) rather than waved through.
        let rules = basic_rules();
        assert!(check_command("make test | tee src/x.rs", &rules, None).is_some());
    }

    #[test]
    fn redirect_denial_message_points_at_edit_tool() {
        let rules = basic_rules();
        let denial = check_command("git status > out.txt", &rules, None).expect("denied");
        let msg = format_denial("git", &rules, &denial, None, None);
        assert!(msg.contains("Edit"), "message names edit tool: {msg}");
        assert!(
            msg.contains("allow_file_redirects"),
            "message names the escape hatch: {msg}",
        );
    }

    // ── mask_quotes boundary tests ──────────────────────────────────

    #[test]
    fn mask_quotes_escaped_double_quote() {
        // Backslash-escaped double quote inside a double-quoted string.
        // The \" should NOT close the quoted region.
        let input = r#"echo "he said \"hello\" today""#;
        let result = mask_quotes(input);
        // "echo " is unquoted — should be preserved.
        assert!(result.starts_with("echo "), "unquoted prefix preserved");
        // Everything inside the quotes should be masked (spaces).
        // The word "hello" is inside quotes — must NOT appear.
        assert!(
            !result.contains("hello"),
            "escaped quote content should be masked, got: {result}",
        );
        // "today" is inside the outer quotes (between \" and closing ").
        assert!(
            !result.contains("today"),
            "content after escaped quote should still be masked, got: {result}",
        );
        assert_eq!(result.len(), input.len(), "length must be preserved");
    }

    #[test]
    fn mask_quotes_unterminated_double_quote() {
        // Unterminated double quote — should mask to end without panic.
        let input = r#"echo "unterminated"#;
        let result = mask_quotes(input);
        assert!(result.starts_with("echo "), "unquoted prefix preserved");
        assert!(!result.contains("unterminated"), "quoted content masked");
        assert_eq!(result.len(), input.len());
    }

    #[test]
    fn mask_quotes_unterminated_single_quote() {
        let input = "echo 'unterminated";
        let result = mask_quotes(input);
        assert!(result.starts_with("echo "), "unquoted prefix preserved");
        assert!(!result.contains("unterminated"), "quoted content masked");
        assert_eq!(result.len(), input.len());
    }

    #[test]
    fn mask_quotes_backslash_at_end_of_double_quote() {
        // Backslash as the last char inside double quotes.
        let input = r#"echo "test\""#;
        let result = mask_quotes(input);
        assert!(result.starts_with("echo "), "unquoted prefix preserved");
        assert!(!result.contains("test"), "quoted content masked");
        assert_eq!(result.len(), input.len());
    }

    // ── pipe_split boundary tests ───────────────────────────────────

    #[test]
    fn pipe_split_trailing_pipe() {
        // Pipe as the last character — tests bounds check on bytes[i+1].
        let parts = pipe_split("echo foo |");
        assert_eq!(parts, vec!["echo foo", ""]);
    }

    #[test]
    fn pipe_split_leading_pipe() {
        // Pipe at position 0 — tests i > 0 guard in backward || check.
        let parts = pipe_split("| grep foo");
        assert_eq!(parts, vec!["", "grep foo"]);
    }

    #[test]
    fn pipe_split_triple_pipe() {
        // ||| = || followed by |. The third | is the second of || in
        // backward check, so it should be skipped (not a bare pipe).
        let parts = pipe_split("a ||| b");
        assert_eq!(parts, vec!["a ||| b"]);
    }

    #[test]
    fn pipe_split_pipe_then_trailing_whitespace() {
        // Pipe followed by trailing whitespace — tests the whitespace
        // skip loop's bounds check (i < n vs i <= n).
        let parts = pipe_split("echo foo |   ");
        assert_eq!(parts, vec!["echo foo", ""]);
    }

    // ── is_unresolvable_cd_target tests ─────────────────────────────

    #[test]
    fn unresolvable_cd_dollar_var() {
        assert!(is_unresolvable_cd_target("$HOME"));
    }

    #[test]
    fn unresolvable_cd_backtick() {
        assert!(is_unresolvable_cd_target("`pwd`"));
    }

    #[test]
    fn unresolvable_cd_command_subst() {
        assert!(is_unresolvable_cd_target("foo$(bar)"));
    }

    #[test]
    fn unresolvable_cd_tilde_user() {
        // ~user (not ~ or ~/path) is unresolvable.
        assert!(is_unresolvable_cd_target("~otheruser"));
    }

    #[test]
    fn resolvable_cd_tilde_alone() {
        assert!(!is_unresolvable_cd_target("~"));
    }

    #[test]
    fn resolvable_cd_tilde_slash() {
        assert!(!is_unresolvable_cd_target("~/projects"));
    }

    #[test]
    fn resolvable_cd_relative_path() {
        assert!(!is_unresolvable_cd_target("src/lib"));
    }

    #[test]
    fn resolvable_cd_absolute_path() {
        assert!(!is_unresolvable_cd_target("/usr/bin"));
    }

    // ── resolve_cd_target tests ─────────────────────────────────────

    #[test]
    fn resolve_cd_dollar_preserves_cwd() {
        let cwd = std::path::Path::new("/current");
        assert_eq!(
            resolve_cd_target("$VAR", Some(cwd)),
            Some(std::path::PathBuf::from("/current")),
        );
    }

    #[test]
    fn resolve_cd_backtick_preserves_cwd() {
        let cwd = std::path::Path::new("/current");
        assert_eq!(
            resolve_cd_target("`cmd`", Some(cwd)),
            Some(std::path::PathBuf::from("/current")),
        );
    }

    #[test]
    fn resolve_cd_command_subst_preserves_cwd() {
        let cwd = std::path::Path::new("/current");
        assert_eq!(
            resolve_cd_target("$(cmd)", Some(cwd)),
            Some(std::path::PathBuf::from("/current")),
        );
    }

    #[test]
    fn resolve_cd_absolute_path() {
        assert_eq!(
            resolve_cd_target("/usr/bin", Some(std::path::Path::new("/tmp"))),
            Some(std::path::PathBuf::from("/usr/bin")),
        );
    }

    #[test]
    fn resolve_cd_relative_resolves_against_cwd() {
        assert_eq!(
            resolve_cd_target("src", Some(std::path::Path::new("/project"))),
            Some(std::path::PathBuf::from("/project/src")),
        );
    }

    // ── Catenary canonical-form matcher (ADR 013 / ticket 16) ────────

    /// The deny reason for `cmd`, or empty string when it was not denied (so the
    /// substring assertions below fail with a readable message instead of
    /// panicking).
    fn deny_text(cmd: &str) -> String {
        match analyze_catenary_command(cmd) {
            CatenaryAction::Deny(m) => m,
            _ => String::new(),
        }
    }

    // ---- Accept table ----

    #[test]
    fn matcher_accepts_bare_search() {
        assert_eq!(
            analyze_catenary_command("catenary grep \"p\" src"),
            CatenaryAction::Allow { has_foreign: false },
        );
        assert_eq!(
            analyze_catenary_command("catenary glob src"),
            CatenaryAction::Allow { has_foreign: false },
        );
    }

    #[test]
    fn matcher_accepts_bare_correlated_and_lifecycle() {
        for cmd in [
            "catenary roots add /tmp/p",
            "catenary roots ls",
            "catenary primer",
            "catenary commands",
        ] {
            assert_eq!(
                analyze_catenary_command(cmd),
                CatenaryAction::Allow { has_foreign: false },
                "{cmd} should be a bare allow",
            );
        }
    }

    #[test]
    fn matcher_denies_commands_pipe_and_chain() {
        // `catenary commands` is lifecycle/bare-only and owns its output.
        assert!(
            matches!(
                analyze_catenary_command("catenary commands | grep git"),
                CatenaryAction::Deny(_),
            ),
            "piping `catenary commands` should deny",
        );
        assert!(deny_text("cd src && catenary commands").contains("its own"));
    }

    #[test]
    fn matcher_routes_bare_sed() {
        // `--in-place` is the write form → identity handoff staged by the hook.
        assert_eq!(
            analyze_catenary_command("catenary sed --in-place a b src"),
            CatenaryAction::Sed { in_place: true },
        );
        // A bare preview is a stateless query → no handoff.
        assert_eq!(
            analyze_catenary_command("catenary sed a b src"),
            CatenaryAction::Sed { in_place: false },
        );
        // Still bare-only: a prefix/chain is denied before the action is read.
        assert!(deny_text("cd src && catenary sed --in-place a b .").contains("its own"));
    }

    #[test]
    fn matcher_routes_editing_lifecycle() {
        assert_eq!(
            analyze_catenary_command("catenary editing start"),
            CatenaryAction::EditingStart,
        );
        assert_eq!(
            analyze_catenary_command("catenary diagnostics"),
            CatenaryAction::Diagnostics,
        );
        assert_eq!(
            analyze_catenary_command("/usr/local/bin/catenary diagnostics"),
            CatenaryAction::Diagnostics,
        );
        assert_eq!(
            analyze_catenary_command("DEBUG=1 catenary editing start"),
            CatenaryAction::EditingStart,
        );
    }

    #[test]
    fn diagnostics_deny_precedes_drain() {
        // Ordering constraint (ticket 11): a piped `catenary diagnostics` must
        // classify as `Deny`, never `Diagnostics`. `run_pre_tool` dispatches
        // `Deny` (print + return) *before* `Diagnostics` (which issues the
        // `pre-tool/editing-stop` prepare that drains the tracked set), so the
        // deny fires first and the set stays intact — never "denied *and*
        // cleared". The bare form still routes to the prepare.
        assert!(
            matches!(
                analyze_catenary_command("catenary diagnostics | head"),
                CatenaryAction::Deny(_)
            ),
            "piped diagnostics must deny before the prepare drains the set",
        );
        assert!(matches!(
            analyze_catenary_command("catenary diagnostics > out.txt"),
            CatenaryAction::Deny(_)
        ));
        assert!(matches!(
            analyze_catenary_command("catenary diagnostics && make test"),
            CatenaryAction::Deny(_)
        ));
        // The bare form is the only one that reaches the drain.
        assert_eq!(
            analyze_catenary_command("catenary diagnostics"),
            CatenaryAction::Diagnostics,
        );
    }

    #[test]
    fn editing_stop_retired() {
        // `editing stop` was renamed to `diagnostics`; a stray invocation is
        // denied with a redirect to the new name in every form — never routed,
        // never a generic "unknown command".
        for cmd in [
            "catenary editing stop",
            "/usr/local/bin/catenary editing stop",
            "catenary editing stop | head",
            "cd src && catenary editing stop",
        ] {
            let msg = deny_text(cmd);
            assert!(
                msg.contains("catenary diagnostics"),
                "{cmd} should redirect to diagnostics, got: {msg}",
            );
        }
    }

    #[test]
    fn matcher_accepts_search_chains() {
        assert_eq!(
            analyze_catenary_command("cd src && catenary grep p"),
            CatenaryAction::Allow { has_foreign: true },
        );
        assert_eq!(
            analyze_catenary_command("catenary grep a && catenary grep b"),
            CatenaryAction::Allow { has_foreign: false },
        );
        assert_eq!(
            analyze_catenary_command("catenary glob x ; catenary glob y"),
            CatenaryAction::Allow { has_foreign: false },
        );
    }

    #[test]
    fn matcher_accepts_arg_substitution() {
        // `$VAR` is not a substitution — bare allow.
        assert_eq!(
            analyze_catenary_command("catenary grep \"$PAT\""),
            CatenaryAction::Allow { has_foreign: false },
        );
        // `$(cmd)` inside an arg is permitted; the inner command is flagged for
        // regime-2 allowlist validation (`has_foreign: true`), not denied here.
        assert_eq!(
            analyze_catenary_command("catenary grep \"$(rg-config)\""),
            CatenaryAction::Allow { has_foreign: true },
        );
    }

    #[test]
    fn matcher_path_prefix_and_quoted_literal() {
        // Path prefix + a literal "catenary" inside the pattern: positional
        // tokenization still resolves the subcommand to grep.
        assert_eq!(
            analyze_catenary_command(r#"/opt/catenary/bin/catenary grep "catenary" src"#),
            CatenaryAction::Allow { has_foreign: false },
        );
    }

    // ---- Deny table: output ownership (both classes) ----

    #[test]
    fn matcher_denies_pipe_out() {
        assert!(deny_text("catenary grep p | head").contains("--page"));
        assert!(deny_text("catenary grep p | wc -l").contains("--count"));
        assert!(deny_text("catenary glob src | tail").contains("--page"));
        // post-filtering structured output
        assert!(deny_text("catenary grep p | grep foo").contains("post-filter"));
    }

    #[test]
    fn matcher_denies_pipe_in() {
        assert!(deny_text("chezmoi managed | catenary grep p").contains("stdin"));
        assert!(deny_text("ls | catenary glob src").contains("stdin"));
    }

    #[test]
    fn matcher_denies_redirect() {
        assert!(deny_text("catenary grep p > out.txt").contains("redirect"));
        assert!(deny_text("catenary diagnostics > out.txt").contains("runtime-dir"));
    }

    #[test]
    fn matcher_denies_background() {
        assert!(deny_text("catenary grep p &").contains("background"));
        assert!(deny_text("catenary editing start &").contains("background"));
    }

    #[test]
    fn matcher_denies_substitution_wrap() {
        assert!(deny_text("$(catenary grep p)").contains("capture"));
        assert!(deny_text("echo `catenary glob src`").contains("capture"));
    }

    // ---- Deny table: bare-only (correlated/lifecycle) ----

    #[test]
    fn matcher_denies_correlated_prefix_and_chain() {
        assert!(deny_text("cd src && catenary diagnostics").contains("its own"));
        assert!(deny_text("catenary diagnostics && make test").contains("its own"));
        assert!(deny_text("make x && catenary sed a b f").contains("its own"));
    }

    #[test]
    fn matcher_denies_two_correlated_in_one_call() {
        let msg = deny_text("catenary sed a b f && catenary diagnostics");
        assert!(msg.contains("on its own line"), "got: {msg}");
    }

    #[test]
    fn matcher_denies_search_mixed_with_correlated() {
        // grep is unrestricted, but diagnostics is not bare → deny.
        assert!(deny_text("catenary grep p && catenary diagnostics").contains("its own"));
    }

    // ---- Deny table: recognition ----

    #[test]
    fn matcher_denies_unknown_subcommand() {
        assert!(deny_text("catenary frobnicate").contains("isn't a recognized"));
        assert!(deny_text("catenary $FOO").contains("isn't a recognized"));
        assert!(deny_text("catenary").contains("isn't a recognized"));
        assert!(deny_text("catenary editing").contains("isn't a recognized"));
    }

    #[test]
    fn matcher_denies_not_agent_invocable() {
        for cmd in [
            "catenary hook pre-tool",
            "catenary stop",
            "catenary debug list",
            "catenary config",
            "catenary daemon",
        ] {
            assert!(
                deny_text(cmd).contains("host CLI hooks"),
                "{cmd} should be not-agent-invocable",
            );
        }
    }

    // ---- Foreign regime unaffected ----

    #[test]
    fn matcher_passes_foreign_through() {
        for cmd in [
            "make test",
            "git status",
            "make x | tail",
            "make test | grep error",
            "git log | grep x",
            "someprog > file.rs",
        ] {
            assert_eq!(
                analyze_catenary_command(cmd),
                CatenaryAction::NotCatenary,
                "{cmd} has no catenary command",
            );
        }
    }

    // ---- bugs/16 regression ----

    #[test]
    fn matcher_bugs16_piped_lifecycle_is_clear_pipe_deny() {
        // A piped lifecycle command yields a clear pipe-deny, not a routed
        // action and not (downstream) the boundary block.
        let msg = deny_text("catenary editing start | head");
        assert!(
            msg.contains("run it bare") || msg.contains("owns its output"),
            "got: {msg}"
        );
        assert!(deny_text("catenary diagnostics | head").contains("preview"));
    }

    // ---- check_command skips catenary segments ----

    #[test]
    fn check_command_skips_catenary_segment() {
        let rules = basic_rules();
        // `catenary` is not in any allowlist, but the foreign filter must skip
        // it (regime 1 owns it) so a search chain's foreign part is validated
        // without `catenary` itself being denied. `echo` is allowed in
        // `basic_rules`; `cd` is not, so use `echo` for the allowed-foreign case.
        assert!(check_command("echo hi && catenary grep p", &rules, None).is_none());
        assert!(check_command("catenary grep p", &rules, None).is_none());
        // The foreign segment is still checked: `cargo` is denied.
        assert!(check_command("cargo build && catenary grep p", &rules, None).is_some());
    }

    // ── Consolidated composition guard (ticket 14) ───────────────────
    //
    // One table over the *combined* filter. It mirrors `run_pre_tool`'s
    // dispatch — regime 1 (`analyze_catenary_command`) first, then regime 2
    // (`check_command`) on the foreign segments — so each row exercises how the
    // rules piled on by tickets 01/02/03/09/11/16 *compose*, not each in
    // isolation. This is the regression guard that no rule masks another.

    #[derive(Debug, PartialEq, Eq)]
    enum Outcome {
        /// The command runs (incl. routed catenary actions that allow it).
        Allow,
        /// Regime 1 (the canonical-form matcher) denied it.
        DenyCatenary,
        /// Regime 2 (the foreign allowlist) denied it, with this reason.
        DenyForeign(DenialReason),
    }

    /// Resolve a command to its combined verdict, mirroring `run_pre_tool`: the
    /// catenary matcher runs first; only `NotCatenary`/`Allow` falls through to
    /// the foreign allowlist, and a canonical search command with foreign
    /// segments still runs them through it.
    fn outcome(cmd: &str, rules: &ResolvedCommands) -> Outcome {
        let foreign = |cmd: &str| {
            check_command(cmd, rules, None)
                .map_or(Outcome::Allow, |d| Outcome::DenyForeign(d.reason))
        };
        match analyze_catenary_command(cmd) {
            CatenaryAction::Deny(_) => Outcome::DenyCatenary,
            CatenaryAction::NotCatenary | CatenaryAction::Allow { has_foreign: true } => {
                foreign(cmd)
            }
            CatenaryAction::Allow { has_foreign: false }
            | CatenaryAction::EditingStart
            | CatenaryAction::Diagnostics
            | CatenaryAction::Sed { .. } => Outcome::Allow,
        }
    }

    #[test]
    fn composition_table() {
        use DenialReason::{NotAllowed, OutputRedirect, PipelinePosition};
        use Outcome::{Allow, DenyCatenary, DenyForeign};
        let rules = recommended_rules();
        let cases: &[(&str, Outcome)] = &[
            // ── Foreign allowlist + redirect gate (01) + reads (03) ──
            ("git status", Allow),
            ("cat src/main.rs", Allow),
            ("git status > out.txt", DenyForeign(OutputRedirect)),
            ("cat foo > bar.rs", DenyForeign(OutputRedirect)),
            ("make test 2>&1", Allow),
            ("make test > /dev/null", Allow),
            (
                "cat <<'EOF' > f.rs\nfn x(){}\nEOF",
                DenyForeign(OutputRedirect),
            ),
            // ── awk/sed out of the pipeline (02) ──
            ("git log | sed -n 'w /tmp/x'", DenyForeign(NotAllowed)),
            ("git log | sort", Allow),
            // ── positional grep-nudge (09/16/bugs19) ──
            ("grep pattern src", DenyForeign(PipelinePosition)),
            ("make test | grep error", Allow),
            ("ls", DenyForeign(NotAllowed)),
            // ── background `&` smuggle CLOSED (ticket 14 / ADR 013) ──
            ("make test & cargo build", DenyForeign(NotAllowed)),
            ("git status & git log", Allow),
            ("make test 2>&1 & git log", Allow),
            ("git commit -m \"fix a & b\"", Allow), // quoted `&` is not a separator
            // ── catenary regime 1: search ──
            ("catenary grep p src", Allow),
            ("catenary grep p | head", DenyCatenary),
            ("catenary grep p | wc -l", DenyCatenary),
            ("chezmoi managed | catenary grep p", DenyCatenary), // bugs/19
            ("cd src && catenary grep p", Allow),
            ("$(catenary grep p)", DenyCatenary),
            ("catenary grep p &", DenyCatenary),
            ("catenary grep a & catenary grep b", DenyCatenary),
            // ── catenary regime 1: correlated/lifecycle (bare-only) ──
            ("catenary diagnostics", Allow),
            ("catenary diagnostics | head", DenyCatenary), // 11: deny before drain
            ("catenary diagnostics && make test", DenyCatenary), // bare-only
            ("catenary diagnostics > out.txt", DenyCatenary),
            ("make x & catenary diagnostics", DenyCatenary), // & seen → bare-only
            ("catenary sed --in-place a b src", Allow),
            ("catenary editing stop", DenyCatenary), // retired → diagnostics
            ("catenary frobnicate", DenyCatenary),
            ("catenary daemon", DenyCatenary), // not agent-invocable
        ];
        for (cmd, want) in cases {
            assert_eq!(&outcome(cmd, &rules), want, "outcome for {cmd:?}");
        }
    }

    // ── Background `&` fix (ticket 14): smuggle closed, care preserved ──

    #[test]
    fn background_amp_denials_are_specific() {
        let rules = recommended_rules();
        // A denied foreign command after `&` is checked, not smuggled.
        let d = check_command("make test & cargo publish", &rules, None)
            .expect("cargo after `&` must be denied");
        assert_eq!(d.reason, DenialReason::NotAllowed);
        // A correlated catenary command after `&` → bare-only guidance.
        assert!(deny_text("make x & catenary diagnostics").contains("its own"));
        // Backgrounding a search command is still denied as backgrounding.
        assert!(deny_text("catenary grep p &").contains("background"));
    }

    #[test]
    fn background_fix_preserves_quotes_and_heredoc() {
        let rules = recommended_rules();
        // A `&` inside a quoted commit message is not a separator.
        assert!(check_command("git commit -m \"fix a & b\"", &rules, None).is_none());
        // The heredoc commit form is untouched: the body (with its `&`) is
        // stripped before splitting, so the `&` fix can't disturb it.
        assert!(
            check_command(
                "git commit -m \"$(cat <<'EOF'\nfix & ship\nEOF\n)\"",
                &rules,
                None
            )
            .is_none(),
            "the `&` fix must not break the git-commit heredoc form",
        );
        // Operators that merely contain `&` are not split.
        assert!(check_command("make test 2>&1", &rules, None).is_none());
        assert!(check_command("make test >&2", &rules, None).is_none());
        assert!(check_command("make test &>/dev/null", &rules, None).is_none());
        // `extract_command_names` sees commands on both sides of a `&`.
        assert_eq!(
            extract_command_names("rm a & cargo build"),
            vec!["rm", "cargo"],
        );
    }

    #[test]
    fn background_amp_inside_substitution_not_split() {
        let rules = recommended_rules();
        // A `&` inside `$(…)` is not a top-level split, so the wrapped catenary
        // command stays intact and is denied with the precise capture message
        // (not a generic NotAllowed on a sliced `$(catenary` token).
        assert!(
            deny_text("$(catenary grep p & foo)").contains("capture"),
            "wrapped catenary keeps the capture message: {}",
            deny_text("$(catenary grep p & foo)"),
        );
        // A denied command inside `$(… & …)` / backticks is still caught via
        // the substitution recursion (which background-splits the inner list).
        assert!(check_command("$(cargo build & make)", &rules, None).is_some());
        assert!(check_command("`cargo build & make`", &rules, None).is_some());
    }

    // ── Newline command separator (ticket 20 / bugs/20) ──────────────
    //
    // A bare newline separates commands in bash (`a\nb` runs both). The `&`
    // sibling of this hole was closed in ticket 14; this closes the newline
    // half. The fix is heredoc-aware: `strip_heredoc_bodies` drops the body
    // *and* the closing-delimiter line, and `newline_split` masks quotes and
    // skips `(…)`/backtick groupings — so a newline inside a quoted arg or the
    // multi-line `git commit` heredoc form is never split. These assertions
    // were the `known_gap_newline_not_a_separator` pins (CatenaryInternal
    // bugs/20); they are now flipped to the closed behavior.

    #[test]
    fn newline_separates_foreign_commands() {
        let rules = recommended_rules();
        // The command after the newline is no longer smuggled: cargo is seen.
        let d = check_command("make test\ncargo build", &rules, None)
            .expect("cargo after a newline must be denied");
        assert_eq!(d.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn newline_surfaces_correlated_catenary_command() {
        // `catenary diagnostics` on a later line is now seen. It shares the call
        // with `make test`, so it is a bare-only violation (not invisible).
        let msg = deny_text("make test\ncatenary diagnostics");
        assert!(
            msg.contains("its own"),
            "diagnostics after a newline must surface as bare-only: {msg:?}",
        );
    }

    #[test]
    fn newline_table() {
        use DenialReason::NotAllowed;
        use Outcome::{Allow, DenyCatenary, DenyForeign};
        let rules = recommended_rules();
        let cases: &[(&str, Outcome)] = &[
            // Two foreign commands on separate lines — the denied one is caught.
            ("make test\ncargo build", DenyForeign(NotAllowed)),
            ("git status\ngit log", Allow),
            // A catenary command on a later line is seen (bare-only deny).
            ("make test\ncatenary diagnostics", DenyCatenary),
            ("foo\ncatenary sed --in-place a b src", DenyCatenary),
            // A newline *inside* a quoted arg is not a separator.
            ("git commit -m \"line one\nline two\"", Allow),
            // A denied command smuggled after a heredoc body is now caught: the
            // closing `EOF` is dropped, so `cargo build` splits out cleanly.
            ("cat <<EOF\nbody\nEOF\ncargo build", DenyForeign(NotAllowed)),
            // The multi-line `git commit` heredoc form still balances — the body
            // newlines live inside the `"…"`, so they are never split.
            ("git commit -m \"$(cat <<'EOF'\nmsg line\nEOF\n)\"", Allow),
        ];
        for (cmd, want) in cases {
            assert_eq!(&outcome(cmd, &rules), want, "outcome for {cmd:?}");
        }
    }

    // ── bugs/17: backtick subcommand inside a quoted prose arg ───────
    //
    // `git commit -m "... `editing start` ..."` is DENIED: a backtick inside
    // double quotes is live command substitution in bash, so the parser
    // recurses and finds `editing` (not allowlisted). This is shell-correct —
    // the backtick WOULD execute at the shell and corrupt the commit — and a
    // carve-out (skipping backtick content inside `-m` args) would be a real
    // hole, since `git commit -m "`cargo publish`"` must stay denied too.
    // Settled by the ticket 14 review: ACCEPT as working-as-designed; agents
    // single-quote the body or use the heredoc commit form. Pinned here.

    #[test]
    fn bugs17_backtick_subcommand_in_commit_message_denied() {
        let rules = recommended_rules();
        assert!(
            check_command("git commit -m \"see `editing start` first\"", &rules, None).is_some(),
            "backtick substitution is live shell — stays denied (shell-correct)",
        );
        // A genuine hazard in the same shape must also stay denied (no carve-out).
        assert!(check_command("git commit -m \"oops `cargo publish`\"", &rules, None).is_some());
        // The same message WITHOUT backticks is prose, not substitution — allowed.
        assert!(
            check_command("git commit -m \"run editing start first\"", &rules, None).is_none(),
            "plain prose mentioning a subcommand is allowed",
        );
    }

    // ── bug-33 false-deny class now allowed (ticket 02) ──────────────
    //
    // The allowlist evaluator validates only the *command-position* words from
    // the faithful parse, never a substring of a quoted argument. These cases
    // false-denied under the ad-hoc tokenizer (innocent words read as
    // commands); the parse keeps them as a single argument, so they are
    // allowed. A command position inside a substitution is still checked.

    #[test]
    fn bug33_single_quote_escape_idiom_allowed() {
        // `git commit -m 'it'\''s done'` → one command `git`; the `'\''`
        // close·escape·reopen idiom keeps the message one argument, so neither
        // `s` nor `done` reaches command position.
        let rules = recommended_rules();
        assert!(
            check_command(r"git commit -m 'it'\''s done'", &rules, None).is_none(),
            "the single-quote escape idiom must not surface `s`/`done` as commands",
        );
    }

    #[test]
    fn bug33_quoted_prose_arg_allowed() {
        // `git commit -m "… catenary diagnostics …"` → one command `git`; the
        // quoted prose is an argument, not an inner command.
        let rules = recommended_rules();
        assert!(
            check_command(
                "git commit -m \"refactor: catenary diagnostics now bare\"",
                &rules,
                None,
            )
            .is_none(),
            "quoted prose must stay an argument, not an inner command",
        );
    }

    #[test]
    fn bug33_comment_is_not_a_command() {
        // `make test  # comment` → one command `make`; the `#` comment is
        // dropped by the parse and never read as an argument-command.
        let rules = recommended_rules();
        assert!(
            check_command("make test  # run the suite", &rules, None).is_none(),
            "an inline comment must not be parsed as a command",
        );
    }

    #[test]
    fn bug33_command_position_in_substitution_still_checked() {
        // `echo $(cargo build)` → denied on `cargo` (the substitution's command
        // position), not on the allowed `echo`.
        let rules = recommended_rules();
        let denial =
            check_command("echo $(cargo build)", &rules, None).expect("cargo inside $() denied");
        assert_eq!(denial.command, "cargo");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }
}
