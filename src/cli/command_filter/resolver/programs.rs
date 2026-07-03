// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Checkable interpreter programs (ws38 ticket 04, decision 026): `awk` and
//! `perl` programs are **data iff they parse into the pure filter / substitution
//! subset** — parsed, not trusted (soundness layer 4). A small DSL's writer
//! constructs are syntactically enumerable, so its programs are checked, not
//! trusted; a Bash-borne writer is still a writer, so we strive to resolve it
//! and deny only when the positive check is impossible.
//!
//! - **awk** — a literal program with no `system()`, no command pipe (`| cmd`
//!   / `cmd | getline`), and no in-program output redirect
//!   (`print`/`printf … > file` / `>> file`) is a **pure filter**: write-set ∅
//!   ([`SegmentClass::NoWrite`]; a shell-level redirect on the command still
//!   composes through the resolver's redirect arm). gawk `-i inplace` records
//!   the file arguments (plus any `INPLACE_SUFFIX` backup side-writes). An
//!   impure or non-literal program is the surgical, construct-naming
//!   [`SegmentClass::Opaque`].
//! - **perl** (`-pe` / `-p -e` / with `-i` / `-i.bak`) — a literal program that
//!   is a `;`-separated run of `s///` / `tr///` / `y///` statements with flags
//!   other than `/e`, and no `system`/backticks/`open`/`print`-to-handle, is a
//!   filter (∅) or, in-place, the file arguments plus `-i.bak` backups. Perl
//!   regex look-around (`(?<=…)`) is expressly in-subset — it is what makes the
//!   `catenary sed` retirement lossless. Beyond the subset or non-literal →
//!   Opaque with a teaching message. An invocation with **no** literal `-e`/`-E`
//!   program — a script-file operand (`perl script.pl`) or a program read from
//!   stdin (bare `perl`) — is denied: perl is allowlisted for its sed role only,
//!   so a script the hook can't see would hide its writes and reads (the
//!   decision 026 gap class). Pure introspection (`-v`/`-V`/`-h`, no file
//!   operands) is the sole exception.
//!
//! Unbounded languages (`python -c`, `ruby -e`, `node -e`) admit no checkable
//! subset; they are not handled here — the resolver's default arm keeps them at
//! the inherited layer-4 boundary and the foreign allowlist denies them by name.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{SegmentClass, State, Unresolved, WriteToolset, expand_list_operand, u};
use crate::cli::command_filter::parse::{SimpleCommand, WordMeta};

// ── awk ──────────────────────────────────────────────────────────────────────

/// The machine-readable construct name of the `awk -f`/`--file` denial — a
/// program the hook can't see. When `awk` is a script host (misc 129), this
/// shape relaxes to the executor boundary instead of denying.
const AWK_PROGRAM_FILE: &str = "awk-program-file";

/// Resolve an `awk`/`gawk`/`mawk`/`nawk` segment: check the program parses into
/// the pure-filter subset, then classify the write-set (∅ for a filter, the
/// file arguments for gawk `-i inplace`). When `name` is an opted-in script host
/// (misc 129), a `-f`/`--file` program the hook can't see relaxes to the
/// unbounded-interpreter executor boundary (`NoWrite`) instead of the audit
/// denial.
pub(super) fn resolve_awk(
    cmd: &SimpleCommand,
    state: &State,
    name: &str,
) -> Result<SegmentClass, Unresolved> {
    let parsed = match parse_awk_argv(cmd) {
        Ok(parsed) => parsed,
        // A script host's program file is the executor boundary: the allowlist
        // governs whether it runs; its in-program writes keep the layer-4
        // accepted boundary — NoWrite.
        Err(unres) if unres.construct == AWK_PROGRAM_FILE && state.script_hosts.contains(name) => {
            return Ok(SegmentClass::NoWrite);
        }
        Err(unres) => return Err(unres),
    };
    if let Some((program, computed)) = &parsed.program {
        if *computed {
            return Err(awk_computed_program());
        }
        // The purity check runs even without `-i`: `awk '{print > "f"}'` writes
        // a file named inside the program — an unattributable write — from a
        // plain preview run, so it must deny regardless of in-place mode.
        check_awk_program(program)?;
    }
    if !parsed.in_place {
        return Ok(SegmentClass::NoWrite);
    }
    let mut writes = BTreeSet::new();
    for (file, meta) in &parsed.files {
        for path in expand_list_operand(file, *meta, state)? {
            if let Some(suffix) = &parsed.inplace_suffix
                && !suffix.is_empty()
            {
                let mut backup = path.clone().into_os_string();
                backup.push(suffix);
                writes.insert(PathBuf::from(backup));
            }
            writes.insert(path);
        }
    }
    Ok(SegmentClass::Recorded(writes))
}

/// The parsed shape of an awk invocation the resolver needs.
#[derive(Default)]
struct AwkParsed {
    /// The program text plus whether any piece was computed (non-literal).
    program: Option<(String, bool)>,
    /// Whether a `-e`/`--source` program flag was seen (positional operands are
    /// then all files/assignments, never the program).
    saw_e: bool,
    /// gawk `-i inplace` / `--include inplace` was requested.
    in_place: bool,
    /// A literal `-v INPLACE_SUFFIX=SUF` backup suffix, if any.
    inplace_suffix: Option<String>,
    /// File operands (after the program), assignments and `-` excluded.
    files: Vec<(String, WordMeta)>,
}

/// Split an awk argv into program / in-place mode / file operands, failing
/// closed on `-f` (unseeable script file), a code-loading `-i`/`-l` extension,
/// and any unmodeled flag.
fn parse_awk_argv(cmd: &SimpleCommand) -> Result<AwkParsed, Unresolved> {
    let mut parsed = AwkParsed::default();
    let argv = &cmd.argv;
    let mut i = 0;
    let mut after_ddash = false;
    while i < argv.len() {
        let arg = &argv[i];
        let meta = cmd.argv_meta.get(i).copied().unwrap_or_default();
        if after_ddash || arg == "-" || !arg.starts_with('-') {
            add_awk_operand(&mut parsed, arg, meta);
            i += 1;
            continue;
        }
        if arg == "--" {
            after_ddash = true;
            i += 1;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            i += parse_awk_long_flag(long, arg, &mut parsed, cmd, i)?;
            continue;
        }
        i += parse_awk_short_cluster(&arg[1..], &mut parsed, cmd, i)?;
    }
    Ok(parsed)
}

/// Handle one awk long flag; returns how many argv slots it consumed.
fn parse_awk_long_flag(
    long: &str,
    arg: &str,
    parsed: &mut AwkParsed,
    cmd: &SimpleCommand,
    i: usize,
) -> Result<usize, Unresolved> {
    let (name, glued) = long
        .split_once('=')
        .map_or((long, None), |(n, v)| (n, Some(v)));
    match name {
        "source" => {
            let (val, vmeta, adv) = take_long_value(cmd, i, glued);
            let computed = vmeta.value_subs || vmeta.live_dollar || vmeta.live_glob;
            add_awk_e_program(parsed, &val, computed);
            Ok(adv)
        }
        "file" => Err(awk_program_file()),
        "field-separator" => {
            let (_, _, adv) = take_long_value(cmd, i, glued);
            Ok(adv)
        }
        "assign" => {
            let (val, _, adv) = take_long_value(cmd, i, glued);
            note_inplace_suffix(parsed, &val);
            Ok(adv)
        }
        "include" => {
            let (val, _, adv) = take_long_value(cmd, i, glued);
            if val == "inplace" {
                parsed.in_place = true;
                Ok(adv)
            } else {
                Err(awk_extension(&val))
            }
        }
        "characters-as-bytes"
        | "traditional"
        | "non-decimal-data"
        | "use-lc-numeric"
        | "optimize"
        | "posix"
        | "re-interval"
        | "sandbox"
        | "bignum"
        | "version"
        | "copyright"
        | "help"
        | "lint"
        | "lint-old" => Ok(1),
        _ => Err(awk_unmodeled_flag(arg)),
    }
}

/// Handle one awk short-flag cluster; returns how many argv slots it consumed.
fn parse_awk_short_cluster(
    cluster: &str,
    parsed: &mut AwkParsed,
    cmd: &SimpleCommand,
    i: usize,
) -> Result<usize, Unresolved> {
    let bytes = cluster.as_bytes();
    let mut ci = 0;
    while ci < bytes.len() {
        match bytes[ci] {
            b'F' | b'v' | b'e' | b'f' | b'i' => {
                let rest = &cluster[ci + 1..];
                let (val, vmeta, adv) = if rest.is_empty() {
                    (
                        cmd.argv.get(i + 1).cloned().unwrap_or_default(),
                        cmd.argv_meta.get(i + 1).copied().unwrap_or_default(),
                        2,
                    )
                } else {
                    (
                        rest.to_string(),
                        cmd.argv_meta.get(i).copied().unwrap_or_default(),
                        1,
                    )
                };
                match bytes[ci] {
                    b'e' => {
                        let computed = vmeta.value_subs || vmeta.live_dollar || vmeta.live_glob;
                        add_awk_e_program(parsed, &val, computed);
                    }
                    b'F' => {}
                    b'v' => note_inplace_suffix(parsed, &val),
                    b'f' => return Err(awk_program_file()),
                    // The only remaining flag in the outer arm's set is `i`.
                    _ => {
                        if val == "inplace" {
                            parsed.in_place = true;
                        } else {
                            return Err(awk_extension(&val));
                        }
                    }
                }
                return Ok(adv);
            }
            // Boolean flags that neither write nor load code.
            b'b' | b'c' | b'C' | b'n' | b'N' | b'O' | b'P' | b'r' | b't' => ci += 1,
            other => return Err(awk_unmodeled_flag(&format!("-{}", other as char))),
        }
    }
    Ok(1)
}

/// The value of a long flag: its glued `=value`, or the next argv slot.
fn take_long_value(
    cmd: &SimpleCommand,
    i: usize,
    glued: Option<&str>,
) -> (String, WordMeta, usize) {
    glued.map_or_else(
        || {
            (
                cmd.argv.get(i + 1).cloned().unwrap_or_default(),
                cmd.argv_meta.get(i + 1).copied().unwrap_or_default(),
                2,
            )
        },
        |v| {
            (
                v.to_string(),
                cmd.argv_meta.get(i).copied().unwrap_or_default(),
                1,
            )
        },
    )
}

/// Append a `-e`/`--source` program piece (gawk concatenates them with a
/// newline), tracking non-literal pieces.
fn add_awk_e_program(parsed: &mut AwkParsed, value: &str, computed: bool) {
    parsed.saw_e = true;
    let entry = parsed.program.get_or_insert_with(|| (String::new(), false));
    if !entry.0.is_empty() {
        entry.0.push('\n');
    }
    entry.0.push_str(value);
    entry.1 = entry.1 || computed;
}

/// Record the gawk `INPLACE_SUFFIX` backup suffix from a literal `-v`/`--assign`
/// value, when present.
fn note_inplace_suffix(parsed: &mut AwkParsed, assignment: &str) {
    if let Some(suffix) = assignment.strip_prefix("INPLACE_SUFFIX=") {
        parsed.inplace_suffix = Some(suffix.to_string());
    }
}

/// Classify an awk operand: the first positional (when no `-e` was given) is
/// the program; the rest are files, with `var=val` assignments and `-` (stdin)
/// excluded from the write-set.
fn add_awk_operand(parsed: &mut AwkParsed, arg: &str, meta: WordMeta) {
    if !parsed.saw_e && parsed.program.is_none() {
        let computed = meta.value_subs || meta.live_dollar || meta.live_glob;
        parsed.program = Some((arg.to_string(), computed));
        return;
    }
    if arg == "-" || is_awk_assignment(arg) {
        return;
    }
    parsed.files.push((arg.to_string(), meta));
}

/// Whether an awk operand is a `var=value` assignment (a name, then `=`), not a
/// file — awk sets the variable before reading, it never edits it.
fn is_awk_assignment(operand: &str) -> bool {
    let Some(eq) = operand.find('=') else {
        return false;
    };
    let name = &operand[..eq];
    !name.is_empty()
        && name
            .bytes()
            .enumerate()
            .all(|(k, b)| b == b'_' || b.is_ascii_alphabetic() || (k > 0 && b.is_ascii_digit()))
}

/// Verify an awk program parses into the pure-filter subset. Strings, regex
/// literals, and `#` comments are masked so a `|` inside `/a|b/` is alternation
/// (not a pipe) and a `>` inside `"x>y"` is text (not a redirect). The denied
/// constructs, each named: `system()`, a command pipe (`| cmd` / `cmd |
/// getline`), and an in-program output redirect (`print`/`printf … >`/`>>`).
fn check_awk_program(program: &str) -> Result<(), Unresolved> {
    let bytes = program.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    // Whether the previous significant token ends an operand, so a following
    // `/` is division rather than the opening of a regex literal.
    let mut prev_operand = false;
    // `(`/`)` nesting depth (`{`/`}` are statement blocks, not expressions).
    let mut paren_depth: i32 = 0;
    // When inside a `print`/`printf` statement, the paren depth at which a
    // top-level `>`/`>>` is an output redirect (a nested `>` is a comparison).
    let mut print_at: Option<i32> = None;
    while i < n {
        match bytes[i] {
            b' ' | b'\t' | b'\r' => i += 1,
            b';' | b'\n' | b'{' | b'}' => {
                print_at = None;
                prev_operand = false;
                i += 1;
            }
            b'(' => {
                paren_depth += 1;
                prev_operand = false;
                i += 1;
            }
            b')' => {
                paren_depth -= 1;
                prev_operand = true;
                i += 1;
            }
            b'#' => {
                while i < n && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'"' => {
                i = skip_awk_string(bytes, i);
                prev_operand = true;
            }
            b'/' if !prev_operand => {
                i = skip_awk_regex(bytes, i);
                prev_operand = true;
            }
            b'|' => {
                // `||` is logical OR; a lone `|` is a command pipe — either
                // `print | cmd` or `cmd | getline`, both unresolvable execs.
                if i + 1 < n && bytes[i + 1] == b'|' {
                    i += 2;
                    prev_operand = false;
                } else {
                    return Err(awk_pipe());
                }
            }
            b'>' => {
                if i + 1 < n && bytes[i + 1] == b'=' {
                    // `>=` is comparison, never a redirect.
                    i += 2;
                } else {
                    if print_at == Some(paren_depth) {
                        return Err(awk_redirect());
                    }
                    i += usize::from(i + 1 < n && bytes[i + 1] == b'>') + 1;
                }
                prev_operand = false;
            }
            b'0'..=b'9' => {
                while i < n && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'.' | b'_'))
                {
                    i += 1;
                }
                prev_operand = true;
            }
            b'$' => {
                prev_operand = false;
                i += 1;
            }
            b']' => {
                prev_operand = true;
                i += 1;
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let start = i;
                while i < n && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                match &program[start..i] {
                    "system" => return Err(awk_system()),
                    "print" | "printf" => {
                        print_at = Some(paren_depth);
                        prev_operand = false;
                    }
                    word if is_awk_keyword(word) => prev_operand = false,
                    _ => prev_operand = true,
                }
            }
            _ => {
                // Any other operator / punctuation ends an operand and opens a
                // regex context for a following `/`.
                prev_operand = false;
                i += 1;
            }
        }
    }
    Ok(())
}

/// Whether an awk word is a control keyword after which a `/` opens a regex
/// (and which does not itself end an operand). Plain identifiers — variables
/// and function names — are operands.
fn is_awk_keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "else"
            | "while"
            | "for"
            | "do"
            | "return"
            | "next"
            | "nextfile"
            | "delete"
            | "in"
            | "getline"
            | "BEGIN"
            | "END"
            | "function"
            | "func"
            | "case"
            | "switch"
            | "default"
            | "break"
            | "continue"
            | "exit"
            | "close"
            | "fflush"
    )
}

/// Skip an awk double-quoted string starting at its opening quote, honoring
/// backslash escapes. Returns the index just past the closing quote.
fn skip_awk_string(bytes: &[u8], mut i: usize) -> usize {
    let n = bytes.len();
    i += 1;
    while i < n {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' | b'\n' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Skip an awk `/…/` regex literal starting at its opening slash, honoring
/// backslash escapes and `[…]` bracket expressions (where `/` is literal).
/// Returns the index just past the closing slash.
fn skip_awk_regex(bytes: &[u8], mut i: usize) -> usize {
    let n = bytes.len();
    i += 1;
    while i < n {
        match bytes[i] {
            b'\\' => i += 2,
            b'[' => {
                i += 1;
                if i < n && bytes[i] == b'^' {
                    i += 1;
                }
                if i < n && bytes[i] == b']' {
                    i += 1;
                }
                while i < n && bytes[i] != b']' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'/' => return i + 1,
            b'\n' => return i,
            _ => i += 1,
        }
    }
    i
}

// ── perl ─────────────────────────────────────────────────────────────────────

/// Resolve a `perl` segment: check the program is a literal pure-substitution
/// run, then classify the write-set (∅ for a filter, the file arguments plus
/// `-i.bak` backups in-place). When `name` is an opted-in script host (misc
/// 129), a script-file operand / bare stdin program relaxes to the
/// unbounded-interpreter executor boundary (`NoWrite`) instead of the audit
/// denial; inline `-e`/`-E` programs still face the substitution audit.
pub(super) fn resolve_perl(
    cmd: &SimpleCommand,
    state: &State,
    name: &str,
) -> Result<SegmentClass, Unresolved> {
    let parsed = parse_perl_argv(cmd)?;
    if !parsed.has_program_flag {
        // perl is allowlisted for its sed role — inline `-e`/`-E` substitutions
        // only. Without a literal program, a script-file operand
        // (`perl script.pl`) or a program read from stdin (bare `perl`) runs
        // code the hook can't see, hiding its writes and reads. Sole exception:
        // pure introspection (`-v`/`-V`/`-h`) with no file operands.
        if parsed.introspection && parsed.files.is_empty() {
            return Ok(SegmentClass::NoWrite);
        }
        // misc 129: when the user opts perl in as a script host, the unseeable
        // script-file / stdin shape relaxes to the executor boundary — the same
        // layer-4 stance `python script.py` keeps. Inline `-e`/`-E` code stays
        // the denied vector below (python-consistent).
        if state.script_hosts.contains(name) {
            return Ok(SegmentClass::NoWrite);
        }
        return Err(if parsed.files.is_empty() {
            perl_stdin_program(&state.toolset)
        } else {
            perl_script_file(&state.toolset)
        });
    }
    if parsed.program_computed {
        return Err(perl_computed_program());
    }
    check_perl_program(&parsed.programs.join("\n"))?;
    let Some(suffix) = &parsed.in_place else {
        // A literal-program filter: write-set ∅.
        return Ok(SegmentClass::NoWrite);
    };
    if suffix.contains('*') {
        return Err(perl_backup_template());
    }
    let mut writes = BTreeSet::new();
    for (file, meta) in &parsed.files {
        for path in expand_list_operand(file, *meta, state)? {
            if !suffix.is_empty() {
                let mut backup = path.clone().into_os_string();
                backup.push(suffix);
                writes.insert(PathBuf::from(backup));
            }
            writes.insert(path);
        }
    }
    Ok(SegmentClass::Recorded(writes))
}

/// The parsed shape of a perl invocation the resolver needs.
#[derive(Default)]
struct PerlParsed {
    /// `-e`/`-E` program pieces (perl concatenates them with a newline).
    programs: Vec<String>,
    /// Whether any program piece was computed (non-literal).
    program_computed: bool,
    /// Whether a `-e`/`-E` program flag was seen.
    has_program_flag: bool,
    /// Whether a `-v`/`-V`/`-h` introspection flag was seen. Pure introspection
    /// (no program, no file operands) is the sole no-`-e` invocation allowed.
    introspection: bool,
    /// `-i[SUFFIX]` backup suffix, if in-place (`""` for a bare `-i`).
    in_place: Option<String>,
    /// File operands (`-` excluded).
    files: Vec<(String, WordMeta)>,
}

/// Split a perl argv into program / in-place mode / file operands, failing
/// closed on module loads (`-M`/`-m`), the debugger, and unmodeled flags.
fn parse_perl_argv(cmd: &SimpleCommand) -> Result<PerlParsed, Unresolved> {
    let mut parsed = PerlParsed::default();
    let argv = &cmd.argv;
    let mut i = 0;
    let mut after_ddash = false;
    while i < argv.len() {
        let arg = &argv[i];
        let meta = cmd.argv_meta.get(i).copied().unwrap_or_default();
        if after_ddash || arg == "-" || !arg.starts_with('-') {
            if arg != "-" {
                parsed.files.push((arg.clone(), meta));
            }
            i += 1;
            continue;
        }
        if arg == "--" {
            after_ddash = true;
            i += 1;
            continue;
        }
        if arg.starts_with("--") {
            return Err(perl_unmodeled_flag(arg));
        }
        i += parse_perl_short_cluster(&arg[1..], &mut parsed, cmd, i)?;
    }
    Ok(parsed)
}

/// Handle one perl short-flag cluster; returns how many argv slots it consumed.
fn parse_perl_short_cluster(
    cluster: &str,
    parsed: &mut PerlParsed,
    cmd: &SimpleCommand,
    i: usize,
) -> Result<usize, Unresolved> {
    let bytes = cluster.as_bytes();
    let mut ci = 0;
    while ci < bytes.len() {
        match bytes[ci] {
            b'e' | b'E' => {
                let rest = &cluster[ci + 1..];
                let (val, vmeta, adv) = if rest.is_empty() {
                    (
                        cmd.argv.get(i + 1).cloned().unwrap_or_default(),
                        cmd.argv_meta.get(i + 1).copied().unwrap_or_default(),
                        2,
                    )
                } else {
                    (
                        rest.to_string(),
                        cmd.argv_meta.get(i).copied().unwrap_or_default(),
                        1,
                    )
                };
                parsed.has_program_flag = true;
                parsed.program_computed = parsed.program_computed
                    || vmeta.value_subs
                    || vmeta.live_dollar
                    || vmeta.live_glob;
                parsed.programs.push(val);
                return Ok(adv);
            }
            b'i' => {
                // The suffix is the rest of this argument only (`-i.bak`), never
                // the next argv slot.
                parsed.in_place = Some(cluster[ci + 1..].to_string());
                return Ok(1);
            }
            b'F' | b'I' => {
                // Field separator / `@INC` path: harmless. Consume a glued
                // value, or the next argv slot.
                return Ok(if cluster[ci + 1..].is_empty() { 2 } else { 1 });
            }
            b'M' | b'm' => return Err(perl_module_load()),
            // Line-ending / record-separator octals: consume trailing digits.
            b'l' | b'0' => {
                ci += 1;
                while ci < bytes.len() && bytes[ci].is_ascii_digit() {
                    ci += 1;
                }
            }
            // Unicode flags: consume trailing letters/digits.
            b'C' => {
                ci += 1;
                while ci < bytes.len() && bytes[ci].is_ascii_alphanumeric() {
                    ci += 1;
                }
            }
            // Introspection flags: version / verbose config / help. A pure
            // introspection run (no program, no file operands) is the sole
            // no-`-e` invocation that stays a filter.
            b'v' | b'V' | b'h' => {
                parsed.introspection = true;
                ci += 1;
            }
            // Boolean flags that neither write nor load code.
            b'p' | b'n' | b'a' | b's' | b'w' | b'W' | b'X' | b'T' | b't' | b'u' | b'U' | b'c'
            | b'g' | b'f' | b'x' => ci += 1,
            other => return Err(perl_unmodeled_flag(&format!("-{}", other as char))),
        }
    }
    Ok(1)
}

/// Verify a perl program is a `;`-separated run of `s///` / `tr///` / `y///`
/// statements — the pure substitution subset. Look-around and back-references
/// ride inside the delimited sections untouched. The `/e` (eval) flag is denied
/// naming it; anything else (a bracketing delimiter, a bareword statement like
/// `system`/`open`/`print`) fails the positive parse.
fn check_perl_program(program: &str) -> Result<(), Unresolved> {
    let bytes = program.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    loop {
        while i < n && (bytes[i].is_ascii_whitespace() || bytes[i] == b';') {
            i += 1;
        }
        if i >= n {
            return Ok(());
        }
        // The substitution operator: `s`, `tr`, or `y`.
        if bytes[i] == b't' && i + 1 < n && bytes[i + 1] == b'r' {
            i += 2;
        } else if bytes[i] == b's' || bytes[i] == b'y' {
            i += 1;
        } else {
            return Err(perl_unverifiable_program());
        }
        // Optional whitespace, then the delimiter.
        while i < n && matches!(bytes[i], b' ' | b'\t') {
            i += 1;
        }
        if i >= n {
            return Err(perl_unverifiable_program());
        }
        let delim = bytes[i];
        if delim.is_ascii_alphanumeric()
            || delim == b'_'
            || matches!(delim, b'(' | b'{' | b'[' | b'<')
        {
            // A bracketing or word-like delimiter is outside the checked subset
            // (paired `s{…}{…}` forms are not modeled) — fail closed.
            return Err(perl_unverifiable_program());
        }
        // Two delimited sections: pattern then replacement, sharing `delim`.
        i = scan_perl_section(bytes, i + 1, delim).ok_or_else(perl_unverifiable_program)?;
        i = scan_perl_section(bytes, i, delim).ok_or_else(perl_unverifiable_program)?;
        // Flags: `/e` (eval) is the one denied writer/executor.
        while i < n && bytes[i].is_ascii_alphabetic() {
            if bytes[i] == b'e' {
                return Err(perl_e_flag());
            }
            i += 1;
        }
        // A statement must end at whitespace, `;`, or end of program.
        if i < n && !bytes[i].is_ascii_whitespace() && bytes[i] != b';' {
            return Err(perl_unverifiable_program());
        }
    }
}

/// Scan one perl-delimited section starting just after its opening delimiter,
/// honoring backslash escapes. Returns the index just past the closing
/// delimiter, or `None` if it is unterminated.
fn scan_perl_section(bytes: &[u8], mut i: usize, delim: u8) -> Option<usize> {
    let n = bytes.len();
    while i < n {
        if bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if bytes[i] == delim {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

// ── Construct-naming denials ─────────────────────────────────────────────────

fn awk_system() -> Unresolved {
    u(
        "awk-system",
        "The awk `system()` call runs a shell command from inside the program, so the \
         hook can't see what files it writes. Run that command directly, or use awk as \
         a plain filter.",
    )
}

fn awk_pipe() -> Unresolved {
    u(
        "awk-pipe",
        "An awk command pipe (`print | \"cmd\"` or `\"cmd\" | getline`) runs a shell \
         command from inside the program, so the hook can't see what files it writes. \
         Drop the pipe and use awk as a plain filter, or run the command directly.",
    )
}

fn awk_redirect() -> Unresolved {
    u(
        "awk-redirect",
        "An in-program awk output redirect (`print`/`printf … > file` or `>> file`) \
         writes a file named inside the program, which the hook can't see to track. \
         Edit files with gawk `-i inplace`, or send output through a shell redirect.",
    )
}

fn awk_program_file() -> Unresolved {
    u(
        AWK_PROGRAM_FILE,
        "`awk -f`/`--file` reads the program from a separate file the hook can't check \
         for commands that run or redirect. Inline the program as a literal argument.",
    )
}

fn awk_computed_program() -> Unresolved {
    u(
        "awk-computed-program",
        "This awk program is assembled at runtime (`$VAR` / `$(…)` / an unquoted glob), \
         so the hook can't check it for commands that run or redirect. Quote a literal \
         program.",
    )
}

fn awk_extension(value: &str) -> Unresolved {
    u(
        "awk-extension",
        format!(
            "gawk `-i {value}` loads an extension that can run arbitrary code the hook \
             can't check — only `-i inplace` (in-place editing) is recognized. Use \
             `-i inplace`, or make the edit with the host's edit tools."
        ),
    )
}

fn awk_unmodeled_flag(flag: &str) -> Unresolved {
    u(
        "awk-unmodeled-flag",
        format!(
            "`awk {flag}` isn't a flag the hook recognizes, so it can't tell the program \
             apart from the files. Use the plain `awk [-F sep] [-v var=val] 'program' \
             file…` form."
        ),
    )
}

fn perl_e_flag() -> Unresolved {
    u(
        "perl-e-flag",
        "The perl `s///e` flag runs the replacement as code, so the hook can't see what \
         files it writes. Drop the `e` flag; a plain `s///` substitution is fine.",
    )
}

fn perl_unverifiable_program() -> Unresolved {
    u(
        "perl-unverifiable-program",
        "The hook couldn't confirm this perl program only substitutes text (the \
         recognized subset: `;`-separated `s///`, `tr///`, `y///` statements with \
         `/`-style delimiters, flags other than `/e`), so it can't tell whether it also \
         writes files. Simplify it, or make the edit with the host's edit tools.",
    )
}

fn perl_computed_program() -> Unresolved {
    u(
        "perl-computed-program",
        "This perl program is assembled at runtime (`$VAR` / `$(…)` / an unquoted glob), \
         so the hook can't check it for writes. Quote a literal `-e` program.",
    )
}

fn perl_script_file(toolset: &WriteToolset) -> Unresolved {
    u(
        "perl-script-file",
        format!(
            "A perl script file (no `-e`/`-E`) runs a program the hook can't see, so it \
             can't tell which files the script writes or reads. {} Running perl as a \
             script host is a user-level `[commands] script_hosts` opt-in.",
            toolset.inplace_hint(),
        ),
    )
}

fn perl_stdin_program(toolset: &WriteToolset) -> Unresolved {
    u(
        "perl-stdin-program",
        format!(
            "Bare perl (no `-e`/`-E`) reads its program from stdin, which the hook can't \
             see, so it can't tell which files the program writes or reads. {} Running \
             perl as a script host is a user-level `[commands] script_hosts` opt-in.",
            toolset.inplace_hint(),
        ),
    )
}

fn perl_module_load() -> Unresolved {
    u(
        "perl-module-load",
        "`perl -M`/`-m` loads a module that can run arbitrary code the hook can't check. \
         Use a plain `-pe 's///'` substitution.",
    )
}

fn perl_backup_template() -> Unresolved {
    u(
        "perl-backup-template",
        "A `perl -i` backup suffix containing `*` is a template that expands to backup \
         paths the hook can't predict. Use a plain suffix like `-i.bak`.",
    )
}

fn perl_unmodeled_flag(flag: &str) -> Unresolved {
    u(
        "perl-unmodeled-flag",
        format!(
            "`perl {flag}` isn't a flag the hook recognizes, so it can't tell the \
             program apart from the files. Use the plain `perl -i[.bak] -pe 's///' \
             file…` form."
        ),
    )
}
