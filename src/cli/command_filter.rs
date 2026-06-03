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

/// Remove heredoc bodies, keeping the marker line and closing delimiter.
///
/// Heredoc bodies are literal text, not shell commands. Without stripping
/// them, the recursive subshell checker would parse their content as
/// commands — triggering false denials on natural language.
fn strip_heredoc_bodies(cmd_string: &str) -> String {
    let mut result = Vec::new();
    let mut skip_until: Option<String> = None;

    for line in cmd_string.split('\n') {
        if let Some(ref marker) = skip_until {
            if line.trim() == marker {
                skip_until = None;
                result.push(line);
            }
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
fn check_against_allowlist(
    name: &str,
    rest: &[&str],
    subcommand: Option<&str>,
    has_heredoc: bool,
    pipe_pos: usize,
    rules: &ResolvedCommands,
    cwd: Option<&std::path::Path>,
) -> Option<(String, DenialReason)> {
    // Heredoc exception: command is reading from stdin, not files.
    if has_heredoc {
        return None;
    }

    // Build tool is always allowed (per-root lookup with default fallback).
    if rules.build_for_cwd(cwd).iter().any(|t| t == name) {
        if let Some(flag) = check_denied_flags(name, rest, rules) {
            return Some((format!("{name} {flag}"), DenialReason::DeniedFlag));
        }
        return None;
    }

    // Check if command is in the unconditional allow list.
    if rules.allow.contains(name) {
        // Check subcommand deny: e.g., git is allowed but `git grep` is denied.
        // Returns the full denied form (e.g., "git grep") for clear denial messages.
        if let Some(sub) = subcommand
            && let Some(denied_subs) = rules.deny.get(name)
            && denied_subs.contains(sub)
        {
            return Some((format!("{name} {sub}"), DenialReason::DeniedSubcommand));
        }
        if let Some(flag) = check_denied_flags(name, rest, rules) {
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
        if let Some(flag) = check_denied_flags(name, rest, rules) {
            return Some((format!("{name} {flag}"), DenialReason::DeniedFlag));
        }
        return None;
    }

    // Not in any allow list — denied.
    Some((name.to_string(), DenialReason::NotAllowed))
}

/// Scan arguments for denied flags.
///
/// Checks `rest[1..]` (everything after the command name) against
/// `deny_flags.<name>`. Long flags with `=` are split (e.g.,
/// `--manifest-path=Cargo.toml` matches `--manifest-path`). Short flags
/// are matched as-is — no combined flag decomposition (`-rf` does not
/// match `-r`).
///
/// Returns the matched flag if found.
fn check_denied_flags(name: &str, rest: &[&str], rules: &ResolvedCommands) -> Option<String> {
    let denied = rules.deny_flags.get(name)?;
    for token in rest.iter().skip(1) {
        let flag = if token.starts_with("--") {
            token.split_once('=').map_or(*token, |(flag, _)| flag)
        } else {
            token
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

    let sequential = quote_aware_split(&cmd_string, &SEQ_SPLIT_RE);
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

            let tokens = shell_split(segment);
            let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
            if token_refs.is_empty() {
                continue;
            }

            let Some(cmd_idx) = find_command(&token_refs) else {
                continue;
            };

            let name = std::path::Path::new(token_refs[cmd_idx])
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(token_refs[cmd_idx]);

            // Output redirection to a file bypasses the tracked Edit/Write
            // path, making the diagnostics batch incomplete. Deny it before
            // the allow/deny decision (and before the heredoc exception) so
            // neither an otherwise-allowed command nor the heredoc
            // short-circuit can carry a redirect through. Gated by
            // `allow_file_redirects`.
            if !rules.allow_file_redirects && redirects_to_file(segment) {
                return Some(Denial {
                    command: name.to_string(),
                    reason: DenialReason::OutputRedirect,
                    unresolved_cd: saw_unresolved_cd,
                    effective_cwd,
                });
            }

            let rest = &token_refs[cmd_idx..];
            // Heredoc exception: only when `<<` is the first argument after
            // the command name. Quoted arguments (like sed patterns) are
            // invisible here because mask_quotes already collapsed them,
            // so `sed 's/foo/bar/' <<EOF` tokenizes as `["sed", "<<EOF"]`.
            // This prevents `rm -rf target/ <<EOF` from bypassing the
            // allowlist while preserving the `cat <<'EOF'` commit pattern.
            let has_heredoc = rest.get(1).is_some_and(|t| t.starts_with("<<"));
            let subcommand = if rest.len() > 1 { Some(rest[1]) } else { None };

            if let Some((denied, reason)) = check_against_allowlist(
                name,
                rest,
                subcommand,
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

            // Track `cd` to update effective cwd for subsequent segments.
            if name == "cd"
                && let Some(target) = subcommand
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
/// Reuses the same parsing infrastructure as [`check_command`]: heredoc
/// stripping, echo separator removal, sequential/pipe splitting, subshell
/// recursion, env-var prefix skipping, and full-path stripping. Returns the
/// bare command names (e.g., `rm`, `cp`) found at each pipeline position.
///
/// Used by editing enforcement to decide whether a Bash tool call contains
/// only filesystem-manipulation commands.
#[must_use]
pub fn extract_command_names(cmd: &str) -> Vec<String> {
    let mut names = Vec::new();
    collect_command_names(cmd, &mut names);
    names
}

/// Return the argument tokens following the command token in a single
/// shell command, joined by single spaces.
///
/// Tokenizes `cmd` with the same quote-aware splitter as
/// [`extract_command_names`] and skips leading `VAR=value` env
/// assignments, so the command token is located positionally rather than
/// by substring search. Returns `None` if there is no command token
/// (e.g. an all-assignments line).
///
/// Hook command recognition uses this to read a Catenary subcommand
/// without being fooled by the command name appearing literally inside a
/// quoted argument — `catenary grep '("catenary")'` must still resolve
/// its subcommand to `grep`, which `str::rfind("catenary")` would not.
/// Quoted arguments are dropped by the tokenizer, so this is only
/// suitable for reading leading subcommand words, not full arguments.
#[must_use]
pub fn args_after_command(cmd: &str) -> Option<String> {
    let tokens = shell_split(cmd);
    let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
    let cmd_idx = find_command(&token_refs)?;
    Some(token_refs[cmd_idx + 1..].join(" "))
}

/// Recursive helper for [`extract_command_names`].
fn collect_command_names(cmd: &str, names: &mut Vec<String>) {
    let cmd_string = strip_heredoc_bodies(cmd);
    let cmd_string = strip_echo_separators(&cmd_string);

    let sequential = quote_aware_split(&cmd_string, &SEQ_SPLIT_RE);
    for seq in sequential {
        let stages = pipe_split(seq);
        for segment in &stages {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }

            // Recursively process $(), <(), and `` substitutions.
            for m in SUBSHELL_RE.captures_iter(segment) {
                let inner = m
                    .get(1)
                    .or_else(|| m.get(2))
                    .or_else(|| m.get(3))
                    .map_or("", |g| g.as_str().trim());
                collect_command_names(inner, names);
            }

            let tokens = shell_split(segment);
            let token_refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
            if token_refs.is_empty() {
                continue;
            }

            let Some(cmd_idx) = find_command(&token_refs) else {
                continue;
            };

            let name = std::path::Path::new(token_refs[cmd_idx])
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(token_refs[cmd_idx]);

            names.push(name.to_string());
        }
    }
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

/// Format the full denial response with the complete allowlist configuration.
///
/// Used on the first denial in a new turn (or after a config change) to give
/// the agent full visibility into its allowed command surface.
///
/// Lists are sorted alphabetically. Sections with no entries are omitted.
/// The denied command is always named in the opening line. `build_hint` is
/// a pre-resolved build guidance string from the caller (when available).
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "redirect guidance expanded the format string"
)]
pub fn format_denial_full(
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

    // Per-root build tools, then default. Each line is explicit about scope.
    let mut root_entries: Vec<(&std::path::Path, &[String])> = commands
        .build
        .iter()
        .map(|(root, tools)| (root.as_path(), tools.as_slice()))
        .collect();
    root_entries.sort_unstable_by_key(|(root, _)| *root);
    for (root, tools) in &root_entries {
        let tool_list: Vec<String> = tools.iter().map(|t| format!("`{t}`")).collect();
        parts.push(format!(
            "Build tool for `{}`: {}",
            root.display(),
            tool_list.join(", ")
        ));
    }
    if !commands.default_build.is_empty() {
        let tool_list: Vec<String> = commands
            .default_build
            .iter()
            .map(|t| format!("`{t}`"))
            .collect();
        parts.push(format!("Default build tool: {}", tool_list.join(", ")));
    }

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

/// Format the short denial response for subsequent denials in the same turn.
///
/// After the full config has been shown once in a turn, subsequent denials
/// use this shorter form to reduce noise. Includes guidance hint when
/// available. `build_hint` is a pre-resolved short build guidance string.
#[must_use]
pub fn format_denial_short(
    denied_cmd: &str,
    denial: &Denial,
    commands: &ResolvedCommands,
    format: Option<super::HostFormat>,
    build_hint: Option<&str>,
) -> String {
    // Output-redirection denial carries the same fixed guidance in both forms.
    if denial.reason == DenialReason::OutputRedirect {
        return format_redirect_denial(format);
    }

    let lookup_cmd = denied_cmd.split_whitespace().next().unwrap_or(denied_cmd);

    let no_guidance = " — see earlier message for the current Catenary command configuration.";
    let suffix = commands.guidance_for(lookup_cmd).map_or_else(
        || String::from(no_guidance),
        |entry| match entry {
            crate::config::GuidanceEntry::Static(msg) => {
                let resolved = resolve_client_vars(msg, format);
                format!(" — {resolved}")
            }
            crate::config::GuidanceEntry::Build(_) => {
                build_hint.map_or_else(|| String::from(no_guidance), |hint| format!(" — {hint}"))
            }
            crate::config::GuidanceEntry::Redirect { command, .. } => {
                format!(
                    " — Use `catenary {command}` instead. \
                     Works on any path (LSP enrichment only within tracked roots)."
                )
            }
        },
    );

    let opening = match denial.reason {
        DenialReason::NotAllowed => format!("`{denied_cmd}` isn't allowed"),
        DenialReason::PipelinePosition => {
            format!("`{denied_cmd}` isn't allowed at the start of a pipeline")
        }
        DenialReason::DeniedSubcommand => {
            format!("`{denied_cmd}` isn't allowed (denied subcommand)")
        }
        DenialReason::DeniedFlag => {
            format!("`{denied_cmd}` isn't allowed (denied flag)")
        }
        // Early-returned above; arm only satisfies exhaustiveness.
        DenialReason::OutputRedirect => {
            format!("`{denied_cmd}` isn't allowed (output redirection)")
        }
    };

    format!("{opening}{suffix}")
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
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

    // ── args_after_command tests ─────────────────────────────────────

    #[test]
    fn args_after_command_simple() {
        assert_eq!(
            args_after_command("catenary grep foo src"),
            Some("grep foo src".to_string())
        );
    }

    #[test]
    fn args_after_command_skips_env_and_path_prefix() {
        assert_eq!(
            args_after_command("DEBUG=1 /usr/local/bin/catenary editing start"),
            Some("editing start".to_string())
        );
    }

    #[test]
    fn args_after_command_ignores_quoted_command_name() {
        // The quoted argument is dropped by the tokenizer, but the
        // subcommand token is still read positionally — not by matching
        // the literal "catenary" inside the quotes.
        assert_eq!(
            args_after_command(r#"catenary grep "catenary" src"#),
            Some("grep src".to_string())
        );
    }

    #[test]
    fn args_after_command_all_env_vars() {
        assert_eq!(args_after_command("A=1 B=2"), None);
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
        let names = extract_command_names("rm $(cat files.txt)");
        assert_eq!(names, vec!["cat", "rm"]);
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
    fn format_full_all_sections() {
        let rules = python_equivalent_rules();
        let msg = format_denial_full("ls", &rules, &no_cd_denial("ls"), None, None);

        assert!(msg.starts_with("`ls` isn't allowed"), "opening line");
        assert!(msg.contains("Allowed:"), "allow section");
        assert!(
            msg.contains("Allowed in pipelines (not first):"),
            "pipeline section"
        );
        assert!(msg.contains("Denied subcommands:"), "deny section");
        assert!(
            msg.contains("Default build tool: `make`"),
            "build section: {msg}",
        );
    }

    #[test]
    fn format_full_multi_build_tools() {
        let rules = ResolvedCommands {
            allow: HashSet::from(["git".into()]),
            default_build: vec!["make".into(), "npm".into()],
            build: HashMap::from([(
                std::path::PathBuf::from("/project"),
                vec!["cargo".into(), "npm".into()],
            )]),
            ..ResolvedCommands::default()
        };
        let msg = format_denial_full("ls", &rules, &no_cd_denial("ls"), None, None);
        assert!(
            msg.contains("Default build tool: `make`, `npm`"),
            "multi default: {msg}",
        );
        assert!(
            msg.contains("Build tool for `/project`: `cargo`, `npm`"),
            "multi per-root: {msg}",
        );
    }

    #[test]
    fn format_full_sorted_alphabetically() {
        let rules = python_equivalent_rules();
        let msg = format_denial_full("ls", &rules, &no_cd_denial("ls"), None, None);

        // Extract the Allowed line and verify sorting.
        let allowed_line = msg
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
    fn format_full_omits_empty_sections() {
        let rules = ResolvedCommands {
            allow: HashSet::from(["git".into()]),
            ..ResolvedCommands::default()
        };
        let msg = format_denial_full("ls", &rules, &no_cd_denial("ls"), None, None);

        assert!(msg.contains("Allowed: git"));
        assert!(
            !msg.contains("Allowed in pipelines"),
            "empty pipeline should be omitted"
        );
        assert!(
            !msg.contains("Denied subcommands"),
            "empty deny should be omitted"
        );
        assert!(
            !msg.contains("Build tool"),
            "absent build should be omitted"
        );
    }

    #[test]
    fn format_full_deny_pairs_sorted() {
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
        let msg = format_denial_full("ls", &rules, &no_cd_denial("ls"), None, None);

        let deny_line = msg
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
    fn format_short_contains_command() {
        let rules = basic_rules();
        let denial = no_cd_denial("cargo");
        let msg = format_denial_short("cargo", &denial, &rules, None, None);
        assert!(msg.contains("`cargo`"));
        assert!(msg.contains("see earlier message"));
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
        let msg = format_denial_full("npm", &rules, &denial, None, None);
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
        let msg = format_denial_full("npm", &rules, &denial, None, None);
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
        let msg = format_denial_full("grep", &rules, &denial, None, None);
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
        let msg = format_denial_full("grep", &rules, &denial, None, None);
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
        let msg = format_denial_full("git grep", &rules, &denial, None, None);
        assert!(
            msg.starts_with("`git grep` isn't allowed (denied subcommand)."),
            "subcommand opening line: {msg}",
        );
    }

    #[test]
    fn format_full_no_guidance_fallback() {
        let rules = basic_rules();
        let denial = no_cd_denial("ls");
        let msg = format_denial_full("ls", &rules, &denial, None, None);
        assert!(
            !msg.contains("Hint:"),
            "no guidance should mean no Hint line: {msg}",
        );
    }

    #[test]
    fn format_full_read_edit_template_vars_default() {
        let rules = rules_with_guidance();
        let denial = no_cd_denial("cat");
        let msg = format_denial_full("cat", &rules, &denial, None, None);
        assert!(
            msg.contains("Hint: Use Read instead"),
            "{{READ}} should resolve to Read by default: {msg}",
        );
    }

    #[test]
    fn format_full_read_edit_template_vars_claude() {
        let rules = rules_with_guidance();
        let denial = no_cd_denial("cat");
        let msg = format_denial_full(
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
        let msg = format_denial_full(
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
        let msg = format_denial_full("cargo", &rules, &denial, None, Some(hint));
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
        let msg = format_denial_full("cargo", &rules, &denial, None, None);
        assert!(
            !msg.contains("Hint:"),
            "no build_hint should mean no Hint line: {msg}",
        );
    }

    #[test]
    fn format_short_with_guidance() {
        let rules = rules_with_guidance();
        let denial = Denial {
            command: "grep".to_string(),
            reason: DenialReason::PipelinePosition,
            unresolved_cd: false,
            effective_cwd: None,
        };
        let msg = format_denial_short("grep", &denial, &rules, None, None);
        assert!(
            msg.contains("Use Catenary's grep tool instead"),
            "short form should include guidance: {msg}",
        );
        assert!(
            msg.contains("at the start of a pipeline"),
            "short form should use pipeline reason: {msg}",
        );
    }

    #[test]
    fn format_short_no_guidance() {
        let rules = basic_rules();
        let denial = no_cd_denial("ls");
        let msg = format_denial_short("ls", &denial, &rules, None, None);
        assert!(
            msg.contains("see earlier message"),
            "no guidance should show fallback: {msg}",
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
        let msg = format_denial_full("make -C", &rules, &denial, None, None);
        assert!(
            msg.starts_with("`make -C` isn't allowed (denied flag)."),
            "flag denial opening: {msg}",
        );
    }

    #[test]
    fn format_full_denied_flags_section() {
        let rules = rules_with_deny_flags();
        let msg = format_denial_full("ls", &rules, &no_cd_denial("ls"), None, None);
        assert!(
            msg.contains("Denied flags:"),
            "should have denied flags section: {msg}",
        );
        assert!(
            msg.contains("cargo --manifest-path"),
            "should list cargo --manifest-path: {msg}",
        );
        assert!(msg.contains("make -C"), "should list make -C: {msg}");
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
        let full = format_denial_full("git", &rules, &denial, None, None);
        assert!(
            full.contains("Edit"),
            "full message names edit tool: {full}"
        );
        assert!(
            full.contains("allow_file_redirects"),
            "full message names the escape hatch: {full}",
        );
        let short = format_denial_short("git", &denial, &rules, None, None);
        assert!(
            short.contains("Edit"),
            "short message names edit tool: {short}"
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
}
