// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! `catenary sed` — the sanctioned, tracked mass-edit surface.
//!
//! Mass edits (project-wide renames) are a real need the Edit tool serves
//! poorly, and raw `sed -i` is denied (cli-prerelease ticket 02) because it
//! bypasses the Edit funnel and makes the diagnostics batch lie. `catenary sed`
//! is the redirect — same pattern as `grep`→`catenary grep` — and because the
//! daemon performs the write it knows the *exact* touched set, which the router
//! feeds straight into the editing-accumulation set (cli-prerelease ticket 08,
//! Decision 10).
//!
//! The engine is the [`regex`] crate (one dialect across search and substitute,
//! shared with `catenary grep`). It is *not* full GNU sed: the interface is
//! positional (`<pattern> <replacement> [paths]`), capture groups are `$1` (not
//! `\1`), and a replacement validator turns the silent sed-escape corruption
//! vector into a loud, grep-style correction. The linear-time `regex` engine
//! (no backreferences/lookaround) guarantees no catastrophic-backtracking hang
//! on an agent-supplied pattern run across a whole repo.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};

use ignore::WalkBuilder;
use regex::{Captures, Regex, RegexBuilder};

use super::pagination::paginate;
use super::session::{ResolvedGlob, path_is_gitignored};

/// VCS metadata directories `catenary sed` never edits.
///
/// Non-overridable: independent of `--include-hidden` / `--include-gitignored`
/// and applied to both the walked set *and* explicitly named paths. A literal
/// path resolving inside one of these is *refused* (not silently skipped); a
/// match swept up by a broad glob is dropped and reported.
const VCS_DIRS: [&str; 4] = [".git", ".jj", ".hg", ".svn"];

/// Maximum file size (bytes) `catenary sed` reads. Larger files are treated as
/// binary and skipped — a regex rewrite of a multi-megabyte blob is far more
/// likely corruption than an intended edit.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// Loud error when `catenary sed` is invoked without an explicit path.
///
/// The one deliberate divergence from grep's cwd-default: a missing path must
/// not mean "rewrite the whole tree under cwd".
pub const REQUIRES_PATH_MSG: &str = "catenary sed needs an explicit path — it will not rewrite the whole tree. \
     Pass a file, directory, or quoted glob pattern, e.g. `catenary sed 'old' \
     'new' 'src/**/*.rs'`.";

/// Parameters for one `catenary sed` invocation, daemon-side.
///
/// Paths are absolute (the CLI resolves every argument against `cwd` before
/// dispatch) and `exclude` is already resolved to an effective glob. Owned so
/// the whole input can move into a `spawn_blocking` task.
#[allow(
    clippy::struct_excessive_bools,
    reason = "orthogonal CLI flags (--in-place/--ignore-case/--preserve-case/\
              --first/--include-gitignored/--include-hidden), 1:1 with clap"
)]
pub struct SedInput {
    /// Search pattern (`regex`-crate dialect, shared with `catenary grep`).
    pub pattern: String,
    /// Replacement text. `$1`/`${name}` reference capture groups, `$0` the whole
    /// match, `$$` a literal `$`; the C-escapes `\n`/`\t`/`\r`/`\\` are
    /// interpreted; sed-style `\1`/`\L` are rejected, and `&` is a literal `&`.
    pub replacement: String,
    /// Absolute paths and glob patterns to resolve into the edit set.
    pub paths: Vec<PathBuf>,
    /// Write the edits in place (`--in-place`). When false, preview only.
    pub in_place: bool,
    /// Case-insensitive matching (`--ignore-case`).
    pub ignore_case: bool,
    /// Case the replacement to match each hit (`--preserve-case`).
    pub preserve_case: bool,
    /// Replace only the first match per file (`--first`); default replaces all.
    pub first: bool,
    /// Effective exclude glob (already `**/`-prefixed / cwd-resolved), or `None`.
    pub exclude: Option<String>,
    /// Include files ignored by `.gitignore`.
    pub include_gitignored: bool,
    /// Include hidden files and directories (never re-exposes VCS dirs).
    pub include_hidden: bool,
    /// Page number for the paged preview (1-based).
    pub page: usize,
}

/// Result of a `catenary sed` invocation.
pub struct SedOutcome {
    /// Rendered preview / write summary for the agent's stdout.
    pub output: String,
    /// Files actually written (empty for a preview). The router accumulates the
    /// LSP-covered subset under the handed-off `(session_id, agent_id)`.
    pub changed: Vec<PathBuf>,
}

impl SedOutcome {
    /// A terminal outcome that wrote nothing — used for validation errors, the
    /// no-path guard, VCS refusals, and the loud-zero result.
    fn message(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            changed: Vec::new(),
        }
    }
}

/// Files dropped from the edit set, by reason (reported, never silent).
#[derive(Default)]
struct Drops {
    /// Explicitly named paths that were gitignored (and `--include-gitignored`
    /// was not set). This is the Decision 9 convergence guard: a shell-expanded
    /// `**/*.rs` arrives as concrete gitignored paths and is dropped here, so it
    /// matches the set a quoted `"**/*.rs"` produces (the gitignore-aware walker
    /// never emits gitignored entries in the first place — there is nothing to
    /// count for that path, and nothing was ever a candidate to drop).
    gitignored: usize,
    /// Skipped because the path lies inside a VCS metadata directory.
    vcs: usize,
    /// Skipped because the file is binary (or too large / unreadable).
    binary: usize,
    /// Skipped because another session holds the cross-session editing
    /// guardrail on that file's root.
    locked: usize,
}

impl Drops {
    /// Renders the drop summary as `7 skipped gitignored, 2 skipped binary`, or
    /// an empty string when nothing was dropped.
    fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.gitignored > 0 {
            parts.push(format!("{} skipped gitignored", self.gitignored));
        }
        if self.vcs > 0 {
            parts.push(format!("{} skipped .git", self.vcs));
        }
        if self.binary > 0 {
            parts.push(format!("{} skipped binary", self.binary));
        }
        if self.locked > 0 {
            parts.push(format!("{} skipped (another session editing)", self.locked));
        }
        parts.join(", ")
    }
}

/// Per-file substitution result.
struct FileHit {
    /// The edited file.
    path: PathBuf,
    /// Number of matches (replacements applied for `--in-place`).
    count: usize,
}

/// Executes a `catenary sed` invocation: validate, resolve, substitute, and
/// (for `--in-place`) write.
///
/// Synchronous and self-contained (only filesystem IO) so the caller can run it
/// on a blocking thread. `budget` is the per-page character budget for the
/// preview. `guard` decides whether a given file may be written (the
/// cross-session editing guardrail) — it is consulted *only* on the
/// `--in-place` write path, so a preview never acquires a lock.
#[must_use]
pub fn execute(input: &SedInput, budget: usize, guard: impl Fn(&Path) -> bool) -> SedOutcome {
    if input.paths.is_empty() {
        return SedOutcome::message(REQUIRES_PATH_MSG);
    }

    // Compile the pattern first (a bad regex fails loudly, like grep's `\|`) so
    // the replacement validator can cross-check capture references against it.
    let regex = match compile(&input.pattern, input.ignore_case) {
        Ok(re) => re,
        Err(e) => return SedOutcome::message(format!("invalid pattern: {e}")),
    };

    // Validate + interpret the replacement before any walk: a bad replacement
    // is a loud correction, not a silent corruption.
    let replacement = match prepare_replacement(&input.replacement) {
        Ok(r) => r,
        Err(hint) => return SedOutcome::message(hint),
    };
    if let Err(hint) = validate_captures(&replacement, &regex) {
        return SedOutcome::message(hint);
    }

    // Resolve the edit set, refusing explicitly named VCS paths.
    let (files, drops) = match resolve_targets(input) {
        Resolved::Refused(msg) => return SedOutcome::message(msg),
        Resolved::Set { files, drops } => (files, drops),
    };

    let mut drops = drops;
    let mut hits: Vec<FileHit> = Vec::new();
    let mut changed: Vec<PathBuf> = Vec::new();

    for path in files {
        let content = match read_text_file(&path) {
            Ok(c) => c,
            Err(ReadSkip::Binary) => {
                drops.binary += 1;
                continue;
            }
            Err(ReadSkip::Skip) => continue,
        };
        let count = count_matches(&regex, &content, input.first);
        if count == 0 {
            continue;
        }
        if input.in_place {
            // Cross-session guardrail: skip files in a root another session is
            // editing — reported, not silently dropped.
            if !guard(&path) {
                drops.locked += 1;
                continue;
            }
            let new = substitute(
                &regex,
                &content,
                &replacement,
                input.first,
                input.preserve_case,
            );
            if atomic_write(&path, new.as_bytes()).is_ok() {
                changed.push(path.clone());
                hits.push(FileHit { path, count });
            }
        } else {
            hits.push(FileHit { path, count });
        }
    }

    let output = render(input, &hits, &drops, budget);
    SedOutcome { output, changed }
}

/// Validates and interprets a replacement string.
///
/// Rejects sed-style capture/case escapes (`\1`–`\9`, `\L \U \l \u \E`) with a
/// hint naming the `regex`-dialect equivalent — turning a silent repo-wide
/// corruption into a grep-style loud correction. Interprets the unambiguous
/// C-escapes (`\n`, `\t`, `\r`, `\\`); a literal backslash-n is `\\n`.
///
/// `&` carries no special meaning in this dialect — it passes through as a
/// literal `&` (the whole match is `$0`, taught by the primer). Capture
/// *references* (`$1`/`$name`) are cross-checked separately by
/// [`validate_captures`], which needs the compiled pattern.
///
/// # Errors
///
/// Returns a pedagogical hint string when the replacement uses a rejected
/// escape.
fn prepare_replacement(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            // `\\` is an escaped backslash; a trailing lone `\` is kept verbatim.
            Some('\\') | None => out.push('\\'),
            Some(d @ '1'..='9') => {
                return Err(format!(
                    "`catenary sed` uses `${d}`, not `\\{d}`, for capture groups."
                ));
            }
            Some(esc @ ('L' | 'U' | 'l' | 'u' | 'E')) => {
                return Err(format!(
                    "`catenary sed` doesn't support sed case escapes (`\\{esc}`). \
                     Use `--preserve-case` to match each hit's casing."
                ));
            }
            // An unknown escape carries no special meaning — keep it literal.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Ok(out)
}

/// Compiles the search pattern with optional case-insensitivity.
///
/// # Errors
///
/// Returns the `regex` compile error string when the pattern is invalid.
fn compile(pattern: &str, ignore_case: bool) -> Result<Regex, regex::Error> {
    RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
}

/// Cross-checks every `$`-capture reference in the (already C-escape-interpreted)
/// replacement against the compiled pattern's groups.
///
/// The `regex` crate expands an out-of-range `$5` or an unknown `$name` to the
/// *empty string*, silently — the `$`-dialect twin of the `\1` corruption
/// [`prepare_replacement`] catches. This walks the replacement with the same
/// `$`-token rules the engine uses (`$$` is a literal `$`; a name is the longest
/// `[0-9A-Za-z_]` run, or `${…}`) and rejects any reference the pattern can't
/// satisfy. `$0` (the whole match) is always valid.
///
/// # Errors
///
/// Returns a pedagogical hint when a reference names a group the pattern lacks.
fn validate_captures(replacement: &str, regex: &Regex) -> Result<(), String> {
    let group_count = regex.captures_len(); // includes group 0 (whole match)
    let names: HashSet<&str> = regex.capture_names().flatten().collect();

    let bytes = replacement.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        // Trailing `$`, or `$$` (literal `$`) — neither is a reference.
        let Some(&next) = bytes.get(i + 1) else { break };
        if next == b'$' {
            i += 2;
            continue;
        }

        let name = if next == b'{' {
            // `${name}` — read to the closing brace.
            let Some(close) = replacement[i + 2..].find('}') else {
                i += 2;
                continue;
            };
            let name = &replacement[i + 2..i + 2 + close];
            i += 2 + close + 1;
            name
        } else {
            // `$name` — the longest `[0-9A-Za-z_]` run.
            let start = i + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end == start {
                // `$` followed by a non-name char — a literal `$`.
                i += 1;
                continue;
            }
            let name = &replacement[start..end];
            i = end;
            name
        };

        if name.is_empty() {
            continue;
        }
        if name.bytes().all(|b| b.is_ascii_digit()) {
            if name.parse::<usize>().is_ok_and(|idx| idx >= group_count) {
                return Err(format!(
                    "`catenary sed`: the pattern has no capture group `${name}` — \
                     use `$$` for a literal `$` (or `${{N}}` to delimit a group)."
                ));
            }
        } else if !names.contains(name) {
            return Err(format!(
                "`catenary sed`: the pattern has no capture group named `${name}` — \
                 use `$$` for a literal `$`."
            ));
        }
    }
    Ok(())
}

/// Atomically writes `content` to `path`: a temp file in the same directory is
/// renamed over the target, so a crash mid-write can never leave a truncated
/// file. The original file's permissions are preserved.
///
/// # Errors
///
/// Returns an IO error if the temp file cannot be created/written or the rename
/// fails (e.g. the parent directory is read-only).
fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    // Same directory ⇒ same filesystem ⇒ the rename is atomic.
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(content)?;
    tmp.flush()?;
    if let Ok(meta) = path.metadata() {
        let _ = tmp.as_file().set_permissions(meta.permissions());
    }
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

/// Counts the matches a substitution would touch (1 at most under `--first`).
fn count_matches(regex: &Regex, content: &str, first: bool) -> usize {
    if first {
        usize::from(regex.is_match(content))
    } else {
        regex.find_iter(content).count()
    }
}

/// Applies the substitution, returning the rewritten content.
///
/// Under `--preserve-case` each hit's casing (all-lower / all-UPPER / Title) is
/// applied to the expanded replacement via a `replace`-closure; mixed casing is
/// left as authored.
fn substitute(
    regex: &Regex,
    content: &str,
    replacement: &str,
    first: bool,
    preserve_case: bool,
) -> String {
    let replacer = |caps: &Captures<'_>| -> String {
        let mut expanded = String::new();
        caps.expand(replacement, &mut expanded);
        if preserve_case {
            let matched = caps.get(0).map_or("", |m| m.as_str());
            apply_case(detect_case(matched), &expanded)
        } else {
            expanded
        }
    };
    if first {
        regex.replacen(content, 1, replacer).into_owned()
    } else {
        regex.replace_all(content, replacer).into_owned()
    }
}

/// Casing class of a matched span, for `--preserve-case`.
#[derive(Clone, Copy)]
enum Case {
    /// All cased letters are lowercase (`omni`).
    Lower,
    /// All cased letters are uppercase (`OMNI`).
    Upper,
    /// First cased letter upper, the rest lower (`Omni`).
    Title,
    /// Anything else (`camelCase`, digits-only) — leave the replacement as-is.
    Mixed,
}

/// Classifies the casing of `s` (ignoring non-alphabetic characters).
fn detect_case(s: &str) -> Case {
    let letters: Vec<char> = s.chars().filter(|c| c.is_alphabetic()).collect();
    let Some((first, rest)) = letters.split_first() else {
        return Case::Mixed;
    };
    if letters.iter().all(|c| c.is_lowercase()) {
        return Case::Lower;
    }
    if letters.iter().all(|c| c.is_uppercase()) {
        return Case::Upper;
    }
    if first.is_uppercase() && rest.iter().all(|c| c.is_lowercase()) {
        return Case::Title;
    }
    Case::Mixed
}

/// Cases `replacement` to match a detected [`Case`].
fn apply_case(case: Case, replacement: &str) -> String {
    match case {
        Case::Lower => replacement.to_lowercase(),
        Case::Upper => replacement.to_uppercase(),
        Case::Title => title_case(replacement),
        Case::Mixed => replacement.to_string(),
    }
}

/// Uppercases the first character and lowercases the rest.
fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
    })
}

/// Outcome of resolving the path arguments into a concrete edit set.
enum Resolved {
    /// An explicitly named path resolves inside a VCS directory — refuse the
    /// whole command loudly (write nothing).
    Refused(String),
    /// The resolved edit set plus the soft/hard filter drop counts.
    Set {
        /// Concrete files to substitute (sorted, deduplicated).
        files: Vec<PathBuf>,
        /// Filter drops to report.
        drops: Drops,
    },
}

/// Resolves the path arguments into the concrete edit set (literal-first,
/// gitignore/hidden aware), counting drops and refusing explicit VCS paths.
fn resolve_targets(input: &SedInput) -> Resolved {
    let mut drops = Drops::default();
    let mut candidates: Vec<PathBuf> = Vec::new();

    for arg in &input.paths {
        match arg.symlink_metadata() {
            Ok(meta) => {
                // An explicitly named path inside a VCS dir is refused, not
                // skipped (distinct from a glob sweep that *includes* one).
                if has_vcs_component(arg) {
                    return Resolved::Refused(format!(
                        "refused: `{}` is inside a version-control directory \
                         (.git/.jj/.hg/.svn) — `catenary sed` never edits those.",
                        arg.display()
                    ));
                }
                if meta.is_dir() {
                    walk_dir(arg, input, &mut candidates);
                } else if meta.is_file() {
                    if !input.include_gitignored && path_is_gitignored(arg) {
                        drops.gitignored += 1;
                    } else {
                        candidates.push(arg.clone());
                    }
                }
            }
            Err(_) => {
                // A non-existent literal is a glob pattern; expand gitignore-
                // and hidden-aware (gitignored entries never enter the set).
                if let Ok(glob) = ResolvedGlob::new(&arg.to_string_lossy()) {
                    candidates.extend(glob.expand(input.include_gitignored, input.include_hidden));
                }
            }
        }
    }

    let exclude = input
        .exclude
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|pat| globset::Glob::new(pat).ok())
        .map(|g| g.compile_matcher());

    let mut seen = HashSet::new();
    let mut files = Vec::new();
    for path in candidates {
        if !seen.insert(path.clone()) {
            continue;
        }
        if exclude.as_ref().is_some_and(|m| m.is_match(&path)) {
            continue;
        }
        // The VCS exclusion applies to the walked set regardless of
        // `--include-hidden` (which would otherwise let the walker descend into
        // `.git`). These arrived via a glob/dir sweep, so they are dropped and
        // reported — not refused.
        if has_vcs_component(&path) {
            drops.vcs += 1;
            continue;
        }
        if !path.is_file() {
            continue;
        }
        files.push(path);
    }
    files.sort();

    Resolved::Set { files, drops }
}

/// Walks a directory into its files, gitignore- and hidden-aware.
fn walk_dir(dir: &Path, input: &SedInput, out: &mut Vec<PathBuf>) {
    for entry in WalkBuilder::new(dir)
        .git_ignore(!input.include_gitignored)
        .hidden(!input.include_hidden)
        .build()
        .flatten()
    {
        if entry.file_type().is_some_and(|t| t.is_file()) {
            out.push(entry.into_path());
        }
    }
}

/// Returns `true` if any path component is a VCS metadata directory.
fn has_vcs_component(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(name) => name.to_str().is_some_and(|n| VCS_DIRS.contains(&n)),
        _ => false,
    })
}

/// Why a file was skipped while reading.
enum ReadSkip {
    /// Binary, too large, or unreadable — counts as a binary drop.
    Binary,
    /// Not a regular file (e.g. a glob-matched directory) — silently skipped.
    Skip,
}

/// Reads a file as UTF-8 text, skipping binary/oversized/unreadable files.
///
/// # Errors
///
/// Returns [`ReadSkip`] describing why the file was not edited.
fn read_text_file(path: &Path) -> Result<String, ReadSkip> {
    let meta = path.metadata().map_err(|_| ReadSkip::Binary)?;
    if !meta.is_file() {
        return Err(ReadSkip::Skip);
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(ReadSkip::Binary);
    }
    let bytes = std::fs::read(path).map_err(|_| ReadSkip::Binary)?;
    if bytes.contains(&0) {
        return Err(ReadSkip::Binary);
    }
    String::from_utf8(bytes).map_err(|_| ReadSkip::Binary)
}

/// Renders the preview / write summary (the no-match case is a loud zero).
fn render(input: &SedInput, hits: &[FileHit], drops: &Drops, budget: usize) -> String {
    if hits.is_empty() {
        return render_zero(input, drops);
    }

    let total: usize = hits.iter().map(|h| h.count).sum();
    let noun = if input.in_place {
        "replacements"
    } else {
        "matches"
    };

    let mut header = String::new();
    let _ = writeln!(header, "{total} {noun} in {} files", hits.len());
    let drop_summary = drops.summary();
    if !drop_summary.is_empty() {
        let _ = writeln!(header, "{drop_summary}");
    }

    let mut list = String::new();
    for hit in hits {
        let _ = writeln!(list, "  {}: {}", hit.path.display(), hit.count);
    }

    // Header (totals + drops) is always shown; the per-file list is paged.
    format!("{header}{}", paginate(&list, budget, input.page.max(1)))
}

/// Renders the loud-zero result: no matches anywhere, plus any filter drops.
fn render_zero(input: &SedInput, drops: &Drops) -> String {
    let mut out = format!("no matches for: {}\n", input.pattern);
    let drop_summary = drops.summary();
    if !drop_summary.is_empty() {
        let _ = writeln!(out, "{drop_summary}");
    }
    out
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;

    /// Builds a `SedInput` with the defaults a bare preview uses.
    fn input(pattern: &str, replacement: &str, paths: Vec<PathBuf>) -> SedInput {
        SedInput {
            pattern: pattern.to_string(),
            replacement: replacement.to_string(),
            paths,
            in_place: false,
            ignore_case: false,
            preserve_case: false,
            first: false,
            exclude: None,
            include_gitignored: false,
            include_hidden: false,
            page: 1,
        }
    }

    #[test]
    fn requires_explicit_path() {
        let outcome = execute(&input("a", "b", Vec::new()), 4000, |_| true);
        assert!(
            outcome.output.contains("explicit path"),
            "got: {}",
            outcome.output
        );
        assert!(outcome.changed.is_empty());
    }

    #[test]
    fn rejects_backslash_capture() {
        let err = prepare_replacement("\\1").expect_err("\\1 must be rejected");
        assert!(err.contains("$1"), "hint must name $1, got: {err}");
    }

    #[test]
    fn ampersand_is_literal() {
        // `&` carries no special meaning in the regex dialect — it passes through.
        assert_eq!(prepare_replacement("a && b").expect("ok"), "a && b");
        assert_eq!(prepare_replacement("&mut x").expect("ok"), "&mut x");
    }

    #[test]
    fn rejects_out_of_range_capture_ref() {
        let re = compile(r"(\w+)", false).expect("compile");
        let err = validate_captures("$2", &re).expect_err("$2 has no group");
        assert!(err.contains("$$"), "hint must teach $$, got: {err}");
    }

    #[test]
    fn rejects_unknown_named_ref() {
        let re = compile(r"(\w+)", false).expect("compile");
        let err = validate_captures("$missing", &re).expect_err("no such named group");
        assert!(err.contains("missing"), "hint names the group, got: {err}");
    }

    #[test]
    fn accepts_valid_capture_refs() {
        let re = compile(r"(?P<key>\w+)=(\w+)", false).expect("compile");
        // $0 (whole match), a named group, a numbered group, and a literal $$.
        validate_captures("$0 $key=$2 costs $$5", &re).expect("all refs valid");
    }

    #[test]
    fn rejects_case_escapes() {
        let err = prepare_replacement("\\Uhi").expect_err("\\U must be rejected");
        assert!(
            err.contains("preserve-case"),
            "hint must point at --preserve-case, got: {err}"
        );
    }

    #[test]
    fn interprets_c_escapes() {
        assert_eq!(prepare_replacement("a\\nb").expect("ok"), "a\nb");
        assert_eq!(prepare_replacement("a\\tb").expect("ok"), "a\tb");
        assert_eq!(prepare_replacement("a\\rb").expect("ok"), "a\rb");
        // An escaped backslash followed by `n` is a literal backslash-n.
        assert_eq!(prepare_replacement("a\\\\nb").expect("ok"), "a\\nb");
    }

    #[test]
    fn preserve_case_three_cases() {
        let re = compile("omni", true).expect("compile");
        let out = substitute(&re, "Omni omni OMNI", "lattice", false, true);
        assert_eq!(out, "Lattice lattice LATTICE");
    }

    #[test]
    fn first_only_replaces_once() {
        let re = compile("x", false).expect("compile");
        assert_eq!(substitute(&re, "x x x", "y", true, false), "y x x");
        assert_eq!(substitute(&re, "x x x", "y", false, false), "y y y");
    }

    #[test]
    fn capture_group_expansion() {
        let re = compile(r"(\w+)=(\w+)", false).expect("compile");
        assert_eq!(substitute(&re, "a=b", "$2=$1", false, false), "b=a");
    }

    #[test]
    fn preview_lists_files_and_counts_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "foo foo bar\nfoo\n").expect("write");

        let outcome = execute(&input("foo", "baz", vec![file.clone()]), 4000, |_| true);
        assert!(outcome.changed.is_empty(), "preview writes nothing");
        assert!(
            outcome.output.contains("a.txt"),
            "lists the file: {}",
            outcome.output
        );
        assert!(
            outcome.output.contains('3'),
            "shows the per-file count: {}",
            outcome.output
        );
        // File untouched.
        assert_eq!(
            std::fs::read_to_string(&file).expect("read"),
            "foo foo bar\nfoo\n"
        );
    }

    #[test]
    fn in_place_writes_and_returns_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "foo foo\n").expect("write");

        let mut sed = input("foo", "baz", vec![file.clone()]);
        sed.in_place = true;
        let outcome = execute(&sed, 4000, |_| true);

        assert_eq!(outcome.changed, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "baz baz\n");
    }

    #[test]
    fn refuses_explicit_git_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).expect("mkdir .git");
        let config = git.join("config");
        std::fs::write(&config, "x = 1\n").expect("write");

        let mut sed = input("x", "y", vec![config.clone()]);
        sed.in_place = true;
        sed.include_hidden = true;
        let outcome = execute(&sed, 4000, |_| true);

        assert!(
            outcome.output.contains("refused"),
            "explicit .git path is refused: {}",
            outcome.output
        );
        assert!(outcome.changed.is_empty());
        assert_eq!(std::fs::read_to_string(&config).expect("read"), "x = 1\n");
    }

    #[test]
    fn never_walks_git_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git = dir.path().join(".git");
        std::fs::create_dir_all(&git).expect("mkdir .git");
        std::fs::write(git.join("config"), "url = a\n").expect("write");
        std::fs::write(dir.path().join("real.txt"), "url = a\n").expect("write");

        // A directory sweep with hidden included must still skip `.git`.
        let mut sed = input("url", "URL", vec![dir.path().to_path_buf()]);
        sed.in_place = true;
        sed.include_hidden = true;
        let outcome = execute(&sed, 4000, |_| true);

        assert_eq!(outcome.changed, vec![dir.path().join("real.txt")]);
        // .git/config untouched.
        assert_eq!(
            std::fs::read_to_string(git.join("config")).expect("read"),
            "url = a\n"
        );
    }

    #[test]
    fn skips_binary_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("blob.bin");
        std::fs::write(&bin, b"foo\0\xff\xfebar").expect("write");

        let mut sed = input("foo", "baz", vec![bin.clone()]);
        sed.in_place = true;
        let outcome = execute(&sed, 4000, |_| true);

        assert!(outcome.changed.is_empty(), "binary file not edited");
        assert!(
            outcome.output.contains("skipped binary"),
            "binary drop reported: {}",
            outcome.output
        );
        assert_eq!(std::fs::read(&bin).expect("read"), b"foo\0\xff\xfebar");
    }

    #[test]
    fn guard_skips_locked_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "foo\n").expect("write");

        let mut sed = input("foo", "bar", vec![file.clone()]);
        sed.in_place = true;
        // The guard denies every write (simulating another session's lock).
        let outcome = execute(&sed, 4000, |_| false);

        assert!(outcome.changed.is_empty(), "locked file not edited");
        assert!(
            outcome.output.contains("another session"),
            "locked drop reported: {}",
            outcome.output
        );
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "foo\n");
    }

    #[test]
    fn atomic_write_preserves_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("script.sh");
        std::fs::write(&file, "echo foo\n").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let mut sed = input("foo", "bar", vec![file.clone()]);
        sed.in_place = true;
        let outcome = execute(&sed, 4000, |_| true);

        assert_eq!(outcome.changed, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "echo bar\n");
        let mode = std::fs::metadata(&file).expect("meta").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "permissions preserved across atomic write"
        );
    }

    #[test]
    fn reports_gitignored_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("mkdir .git");
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").expect("write");
        let ignored = dir.path().join("ignored.txt");
        std::fs::write(&ignored, "foo\n").expect("write");

        // Explicitly named gitignored file → dropped and reported.
        let outcome = execute(&input("foo", "baz", vec![ignored]), 4000, |_| true);
        assert!(
            outcome.output.contains("skipped gitignored"),
            "gitignored drop reported: {}",
            outcome.output
        );
    }

    #[test]
    fn loud_zero_on_no_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "nothing here\n").expect("write");

        let outcome = execute(&input("absent", "x", vec![file]), 4000, |_| true);
        assert!(
            outcome.output.contains("no matches for: absent"),
            "loud zero: {}",
            outcome.output
        );
    }

    #[test]
    fn invalid_pattern_is_loud() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x\n").expect("write");

        let outcome = execute(&input("(unclosed", "y", vec![file]), 4000, |_| true);
        assert!(
            outcome.output.contains("invalid pattern"),
            "bad regex is loud: {}",
            outcome.output
        );
    }
}
