// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Shell command parser for allowlist-based command filtering.
//!
//! Checks Bash commands against a [`ResolvedCommands`] allowlist. Both the
//! allowlist evaluator and the redirect guard read one faithful
//! [`parse::ParsedScript`] (decision 020 §3): pipeline position is the index
//! into a pipeline's commands, substitutions are recursed, and env-var prefix
//! skipping / path stripping / subcommand deny matching run on the parse's
//! command-position words. A heredoc is stdin input, not an allow/deny knob —
//! the command faces the allowlist on its name alone — but an *unquoted* body's
//! `$(…)` / `` `…` `` command substitutions are expanded and run by the shell, so
//! the parse projects them as command positions on the heredoc-owning command
//! (bug 46); a quoted delimiter's body stays inert stdin.

#[allow(
    clippy::expect_used,
    reason = "all patterns are string literals verified by tests — no user input"
)]
mod patterns {
    use regex::Regex;
    use std::sync::LazyLock;

    /// Matches heredoc start markers: `<<EOF`, `<<'EOF'`, `<<"EOF"`, `<<-EOF`.
    pub static HEREDOC_MARKER_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"<<-?\s*\\?['""]?(\w+)['""]?"#).expect("constant pattern"));

    /// Matches env var assignment prefix: `VAR=value`.
    pub static ENV_VAR_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Za-z_][A-Za-z_0-9]*=").expect("constant pattern"));
}

/// Faithful, hand-rolled shell parser core (`&str → ParsedScript`).
///
/// The parse substrate from decision 020 / tokenizer ticket 01. The allowlist
/// evaluator ([`check_command`]), the redirect guard, and the catenary
/// canonical-form matcher all read it — segmentation, substitution recursion,
/// heredoc stripping, and command-position extraction come from this one parse,
/// not the ad-hoc scanners it replaced (tickets 02 / 03 / 04 / 08).
pub(crate) mod parse;

/// Differential fuzzing oracle (tokenizer ticket 05): `brush-parser` reference +
/// `proptest`.
///
/// Dev/fuzz-only — gated `#[cfg(any(test, feature = "fuzzing"))]`, so the
/// reference parser never enters the runtime / `cargo deny` runtime graph and
/// never ships. The `fuzzing` feature lets the out-of-tree `fuzz/` crate
/// (tokenizer ticket 06) reuse the same `check()` the `proptest` layer drives.
#[cfg(any(test, feature = "fuzzing"))]
pub mod oracle;

/// Write resolver (ws38 ticket 01, decision 026).
///
/// Classifies every segment into Recorded / PureDelete / NoWrite / Opaque.
/// The foreign-redirect gate reads it — a resolvable write is allowed (its
/// complete write-set produced for attribution, ticket 02), an opaque one
/// denies with a teaching message.
pub mod resolver;

/// Command tiers — Read / Edit / Stateful (root-ownership stage 5).
///
/// Classifies a shell command line onto the three lock tiers as DATA over the
/// grant lists: a read passes any window, an edit/stateful command requires the
/// lock. The stateful set (mutating git subcommands, `build`, `chmod`) is the
/// annotation the reconcile bracket also keys off — `git stash`/`checkout`
/// unbook, `git stash pop`/`merge`/`rebase` book.
pub mod tier;

use crate::cli::HostFormat;
use crate::config::ResolvedCommands;

/// Device sinks that are never file writes when they appear as redirect
/// targets (`> /dev/null`). The write resolver skips them when producing a
/// segment's write-set.
const DEVICE_SINKS: [&str; 3] = ["/dev/null", "/dev/stdout", "/dev/stderr"];

/// Whether a single [`Redirect`](parse::Redirect) writes a non-device-sink file.
///
/// Maps the parsed operator + target to the file-write decision (decision 020
/// §8a). Input-side operators (`<`, `<&`, `<<<`) never write. `>` / `>>` / `&>`
/// always write a file. `>&` (`DupOut`) writes a file **only** when its target
/// is a path: a bare fd number (`>&1`) or close (`>&-`) is a descriptor
/// duplication, not a write. A [`DEVICE_SINKS`] target is allowed. Any other
/// target — including an empty target (a dangling `>` or an output process
/// substitution `> >(cmd)`, whose inner command is gated through the
/// substitution recursion) and an unverifiable `$var` / `$(...)` target — is a
/// deny, the fail-closed direction.
fn redirect_writes_file(redirect: &parse::Redirect) -> bool {
    use parse::RedirectOp;

    match redirect.op {
        // Input-side operators never write a file.
        RedirectOp::Read | RedirectOp::DupIn | RedirectOp::HereString => false,
        // Plain / append / combined-stream / read-write output writes a file.
        // `<>` opens the target writable (fd 0 by default), so it must route
        // through the edit path like any other write (bug 41).
        RedirectOp::Write | RedirectOp::Append | RedirectOp::WriteBoth | RedirectOp::ReadWrite => {
            !target_is_device_sink(&redirect.target)
        }
        // `>&word`: a bare fd number (`>&1`) or close (`>&-`) is a descriptor
        // duplication, not a write; a path target (`>&file`) is a file write.
        RedirectOp::DupOut => {
            !is_fd_dup_target(&redirect.target) && !target_is_device_sink(&redirect.target)
        }
    }
}

/// Whether a redirect target is one of the allowed [`DEVICE_SINKS`].
fn target_is_device_sink(target: &str) -> bool {
    DEVICE_SINKS.contains(&target)
}

/// Whether a `>&` target is a file-descriptor duplication / close rather than a
/// file path: an all-digit run (`2`, `10`) or a bare `-` (close).
fn is_fd_dup_target(target: &str) -> bool {
    target == "-" || (!target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()))
}

/// Check whether a command is denied by the allowlist rules.
///
/// A command is denied if:
/// 1. It is not in `allow` or `pipeline` (and not a `build` tool).
/// 2. It is in `pipeline` but at pipe position 0.
/// 3. It is in `allow` but the specific subcommand is in `deny.<cmd>`.
/// 4. It is otherwise allowed but uses a flag in `deny_flags.<cmd>`.
/// 5. It has an `allow_flags.<cmd>` entry but the invocation matches none of
///    the permitted forms (the allow-side form lever; `deny`/`deny_flags`
///    take precedence, checked first). This is policy only — the write
///    resolver's own soundness denials run afterward regardless.
///
/// Returns the denied command name and reason if denied, `None` if allowed.
///
/// `cmd` is a [`SimpleCommand`](parse::SimpleCommand) from the faithful parse
/// (ticket 02): its command-position `name` and `argv` come from the
/// quote-faithful tokenizer, so a substring of a quoted argument can never be
/// mistaken for a command word. The decision ladder — build tool →
/// unconditional allow + denied subcommand/flags → pipeline with the position-0
/// guard → default deny — faces the command on its name alone: a heredoc is
/// stdin input, never an allow/deny knob (decision 020 §2), so `cat <<EOF` is
/// allowed because `cat` is allowed and `python <<EOF` denied because `python`
/// is denied — the form is irrelevant. A command with no command-position word
/// (`name == None`) is not validated.
fn check_against_allowlist(
    cmd: &parse::SimpleCommand,
    pipe_pos: usize,
    rules: &ResolvedCommands,
    cwd: Option<&std::path::Path>,
) -> Option<(String, DenialReason)> {
    // No command-position word (only assignments / redirects) — nothing to
    // validate.
    let name = cmd.name.as_deref()?;
    let argv = &cmd.argv;

    // Build tool is always allowed (per-root lookup with default fallback).
    if rules.build_for_cwd(cwd).iter().any(|t| t == name) {
        if let Some(flag) = check_denied_flags(name, argv, rules) {
            return Some((format!("{name} {flag}"), DenialReason::DeniedFlag));
        }
        if is_disallowed_form(name, argv, rules) {
            return Some((name.to_string(), DenialReason::DisallowedForm));
        }
        return None;
    }

    // Check if command is in the unconditional allow list.
    if rules.allow.contains(name) {
        // Check subcommand deny: e.g., git is allowed but `git grep` is denied.
        // The subcommand is the first positional past the command's global
        // options, resolved flag-aware so a value-carrying global (`git -C
        // <path> grep`, `sqlite3 -readonly -cmd …`) cannot shuffle the real
        // subcommand out of the matched position (bug 140). Returns the full
        // denied form (e.g., "git grep") for clear denial messages.
        if let Some(denied_subs) = rules.deny.get(name)
            && let Some(sub) = cmd.denied_subcommand(denied_subs)
        {
            return Some((format!("{name} {sub}"), DenialReason::DeniedSubcommand));
        }
        if let Some(flag) = check_denied_flags(name, argv, rules) {
            return Some((format!("{name} {flag}"), DenialReason::DeniedFlag));
        }
        if is_disallowed_form(name, argv, rules) {
            return Some((name.to_string(), DenialReason::DisallowedForm));
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
        if is_disallowed_form(name, argv, rules) {
            return Some((name.to_string(), DenialReason::DisallowedForm));
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

/// Cluster-normalize one flag token into its atoms for `allow_flags` matching.
///
/// A short cluster decomposes into per-character atoms (`"-pe"` → `["p", "e"]`),
/// stopping at the first non-alphanumeric byte so a glued value or suffix is
/// ignored (`"-i.bak"` → `["i"]`). A long flag is a single atom, `=value`
/// stripped (`"--in-place=.bak"` → `["--in-place"]`). A bare `-` or a
/// non-flag token (a positional) yields nothing. Short atoms are single-char
/// strings and long atoms carry their `--` prefix, so the two namespaces never
/// collide.
fn form_atoms(token: &str) -> Vec<String> {
    let Some(rest) = token.strip_prefix('-') else {
        return Vec::new(); // positional, not a flag
    };
    if rest.is_empty() {
        return Vec::new(); // bare `-` (stdin marker)
    }
    if let Some(long) = rest.strip_prefix('-') {
        let flag = long.split_once('=').map_or(long, |(f, _)| f);
        if flag.is_empty() {
            return Vec::new(); // bare `--`
        }
        return vec![format!("--{flag}")];
    }
    rest.chars()
        .take_while(char::is_ascii_alphanumeric)
        .map(|c| c.to_string())
        .collect()
}

/// The `allow_flags` form lever: whether a command with an `allow_flags.<name>`
/// entry was invoked in a form that matches **none** of the permitted forms.
///
/// Each listed form is a positive anchor: cluster-normalized to its atoms
/// (`"-pe"` ≡ `{p, e}`), it matches when the invocation *carries all* of those
/// atoms — extra flags beyond the anchor do not disqualify the match (they stay
/// governed by the write resolver's own modeling). Long and short forms are
/// distinct atoms. A command with no entry is inert (returns `false`); a
/// degenerate form that normalizes to no atoms never matches (fail closed).
///
/// This is a pure narrowing gate — when it passes, the write resolver still
/// runs and can still deny (so a soundness denial like the misc-126
/// script-file/stdin-program shape is never re-opened by a listed form).
fn is_disallowed_form(name: &str, argv: &[String], rules: &ResolvedCommands) -> bool {
    let Some(forms) = rules.allow_flags.get(name) else {
        return false; // no entry — lever inert
    };
    let carried: std::collections::HashSet<String> =
        argv.iter().flat_map(|token| form_atoms(token)).collect();
    let matched = forms.iter().any(|form| {
        let required = form_atoms(form);
        !required.is_empty() && required.iter().all(|atom| carried.contains(atom))
    });
    !matched
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
    /// Command has an `allow_flags.<cmd>` entry but the invocation matches none
    /// of the permitted forms. The config-sourced teaching message (naming the
    /// permitted forms) is built in [`format_denial`].
    DisallowedForm,
    /// The command writes through a form the write resolver cannot see
    /// completely (ws38 ticket 01, decision 026): the complete-or-deny
    /// contract denies rather than under-record. The teaching message rides
    /// [`Denial::message`].
    OpaqueWrite,
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
    /// The construct-naming teaching message for an
    /// [`OpaqueWrite`](DenialReason::OpaqueWrite) denial.
    pub message: Option<String>,
    /// Resolved write targets of statements *earlier* in the compound than the
    /// denied one (misc 206, bug 117's mechanism): the denial stops the whole
    /// command, so these writes never happened — [`format_denial`] names them
    /// so the agent doesn't read a stale file as fresh output. Empty for a
    /// single-statement denial, a denial in the first statement, earlier
    /// statements with no write targets, or earlier writes that don't resolve
    /// (never invent paths).
    pub skipped_writes: Vec<std::path::PathBuf>,
}

/// The session class the filter judges a command for (misc 221).
///
/// The branch guard is scoped to **subagent** sessions only — the top-level /
/// lead agent is explicitly untouched (maintainer ruling 2026-07-23). The daemon
/// knows worktree-anchored subagents through the `WorktreeCreate` anchoring, but
/// the command filter runs entirely client-side in the `PreToolUse` hook (no
/// daemon round-trip; enforcement keys are user-level). What the hook *does*
/// carry is the identity that distinguishes the two classes: a non-empty
/// `agent_id` in the hook payload is a subagent, an empty one is the main/lead
/// agent (`extract_agent_id`). This context carries that distinction plus the
/// subagent's anchored worktree (its hook `cwd`) into the filter, so the guard
/// can honor the subagents-only scope without inventing daemon state.
///
/// [`Lead`](Self::Lead) is the default and the untouched path: every existing
/// caller ([`check_command`], the tests) judges as a lead, so the guard is inert
/// unless a caller explicitly threads a [`Subagent`](Self::Subagent) context.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SessionContext {
    /// The top-level / lead agent (or any caller with no session identity). The
    /// branch guard never fires — the lead is explicitly out of scope.
    #[default]
    Lead,
    /// A worktree-anchored subagent, carrying the anchor its branch work belongs
    /// to (the subagent's hook `cwd`). The branch guard denies branch
    /// manipulation targeting a repo outside this anchor.
    Subagent {
        /// The subagent's anchored worktree — its hook `cwd`. A branch operation
        /// whose target repo lies outside this path is denied. `None` when the
        /// hook carried no cwd (fail-open for the guard: with no anchor to
        /// compare against, an outside-target cannot be established).
        anchor: Option<std::path::PathBuf>,
    },
}

impl SessionContext {
    /// The subagent's anchor, or `None` for a lead (the guard is inert).
    fn subagent_anchor(&self) -> Option<&std::path::Path> {
        match self {
            Self::Lead => None,
            Self::Subagent { anchor } => anchor.as_deref(),
        }
    }
}

/// Check all commands in a shell command string against the allowlist rules.
///
/// `cwd` is used for per-root `build` tool lookup. Pass `None` when no
/// working directory is available (falls back to the user-level default
/// build tool).
///
/// Judged as a [lead session](SessionContext::Lead): the subagent branch guard
/// (misc 221) is inert. Use [`check_command_in_session`] to judge a subagent.
///
/// Returns a [`Denial`] for the first denied command, or `None` if all
/// commands are allowed.
#[must_use]
pub fn check_command(
    cmd: &str,
    rules: &ResolvedCommands,
    cwd: Option<&std::path::Path>,
) -> Option<Denial> {
    check_and_resolve_command(cmd, rules, cwd).err()
}

/// Check a command as a specific [`SessionContext`] (misc 221).
///
/// The session-aware twin of [`check_command`]: a [subagent](SessionContext::Subagent)
/// context arms the branch guard (branch manipulation targeting a repo outside
/// the subagent's anchor is denied), a [lead](SessionContext::Lead) context
/// leaves it inert. Discards the write-set — use
/// [`check_and_resolve_command_in_session`] to keep it.
///
/// Returns a [`Denial`] for the first denied command, or `None` if all
/// commands are allowed.
#[must_use]
pub fn check_command_in_session(
    cmd: &str,
    rules: &ResolvedCommands,
    cwd: Option<&std::path::Path>,
    session: &SessionContext,
) -> Option<Denial> {
    check_and_resolve_command_in_session(cmd, rules, cwd, session).err()
}

/// Check the command against the allowlist **and** resolve its write-set.
///
/// The write-carrying twin of [`check_command`]: `Ok(writes)` means the
/// command is allowed and `writes` is the complete set of paths it will write
/// (ticket 02 attributes these into the issuing session's modified-set);
/// `Err(denial)` means the command is denied — either by the foreign allowlist
/// or because a write is opaque (the complete-or-deny contract, ws38 ticket 01,
/// decision 026). `check_command` discards the write-set;
/// [`crate::cli::hooks::run_pre_tool`] keeps it.
///
/// Judges as a [lead session](SessionContext::Lead): the subagent branch guard
/// (misc 221) is inert. Use [`check_and_resolve_command_in_session`] to judge a
/// subagent.
///
/// # Errors
///
/// Returns the first [`Denial`] in document order (allowlist violation before
/// an opaque-write denial for the same line, since the allowlist walk runs
/// first).
pub fn check_and_resolve_command(
    cmd: &str,
    rules: &ResolvedCommands,
    cwd: Option<&std::path::Path>,
) -> Result<resolver::LineWrites, Denial> {
    check_and_resolve_command_in_session(cmd, rules, cwd, &SessionContext::Lead)
}

/// The session-aware core of [`check_and_resolve_command`] (misc 221).
///
/// Identical to [`check_and_resolve_command`] except it judges the command for a
/// given [`SessionContext`]: a subagent context arms the branch guard (see
/// [`check_parsed_command`]). The `session` is threaded down the allowlist walk;
/// the resolve-or-deny write pass is session-agnostic.
///
/// # Errors
///
/// Returns the first [`Denial`] in document order (branch guard / allowlist
/// violation before an opaque-write denial for the same line, since the
/// allowlist walk runs first).
pub fn check_and_resolve_command_in_session(
    cmd: &str,
    rules: &ResolvedCommands,
    cwd: Option<&std::path::Path>,
    session: &SessionContext,
) -> Result<resolver::LineWrites, Denial> {
    // One faithful parse drives segmentation end-to-end (decision 020 §3): the
    // list operators (`;` / `&&` / `||` / newline / `&`) separate
    // `ParsedScript.pipelines`, `|` separates `Pipeline.commands` — so the pipe
    // position is the index into `commands`, with no second ad-hoc scan. Heredoc
    // bodies, `#` comments, and `\`-newline joins are already resolved by the
    // parse, so body prose never reaches the gate.
    let script = parse::parse(cmd);

    // Track effective cwd across the script's commands (document order) for
    // per-root build tool resolution. Updated when `cd <path>` is encountered.
    let mut cwd_state = CwdState {
        effective_cwd: cwd.map(std::path::PathBuf::from),
        saw_unresolved_cd: false,
    };
    if let Some((denied_stmt, mut denial)) = check_script(&script, rules, &mut cwd_state, session) {
        // A denied compound never says the earlier write leg didn't run
        // (misc 206, bug 117): when statements *before* the denied one carry
        // write targets, resolve them — the same resolution the gate would
        // have recorded had the command run — so the denial can name the
        // writes that never happened. Resolution failure names nothing
        // (never invent paths); prefix resolution is identical to the full
        // script's for those statements, since state threads document-order.
        if denied_stmt > 0 {
            let toolset = resolver::WriteToolset::from_allowed(|tool| rules.allow.contains(tool));
            let script_hosts =
                resolver::ScriptHosts::from_names(rules.script_hosts.iter().cloned());
            let earlier = parse::ParsedScript {
                pipelines: script.pipelines[..denied_stmt].to_vec(),
            };
            if let Ok(skipped) = resolver::resolve_script_with(&earlier, cwd, toolset, script_hosts)
            {
                denial.skipped_writes = skipped.writes.into_iter().collect();
            }
        }
        return Err(denial);
    }

    // Resolve-or-deny (ws38 ticket 01, decision 026): every write the command
    // performs must resolve to its complete target set, or the command is
    // denied with a construct-naming teaching message. This replaces the
    // blanket bug-11 foreign-redirect denial — a resolvable redirect (or
    // `cp`/`mv`/`tee`/`sed -i`/`rsync` write) is now allowed, and its resolved
    // write-set flows to attribution (ticket 02). The write model is the
    // design's, not per-user config (the `allow_file_redirects` knob is
    // retired, ticket 05). Catenary's own segments get the same treatment — the
    // canonical-form matcher owns their allow/deny shape, the resolver their
    // write-set.
    // A denial's sanctioned-proceed clause names another shell tool only when
    // the live allowlist permits it (else it points at the host edit tools) —
    // so the fix never bounces the agent into a second denial. Writers run at
    // position 0, so `allow` (not `pipeline`) is the relevant membership.
    let toolset = resolver::WriteToolset::from_allowed(|tool| rules.allow.contains(tool));
    // The user's script-host opt-in (misc 129): a listed `perl`/`awk`/`sed`'s
    // program-file / bare form resolves at the executor boundary instead of the
    // audit denial. Empty (the default) leaves every modeled engine's soundness
    // denial exactly as it was.
    let script_hosts = resolver::ScriptHosts::from_names(rules.script_hosts.iter().cloned());
    match resolver::resolve_script_with(&script, cwd, toolset, script_hosts) {
        Ok(writes) => Ok(writes),
        Err(opaque) => Err(Denial {
            command: opaque.command,
            reason: DenialReason::OpaqueWrite,
            unresolved_cd: cwd_state.saw_unresolved_cd,
            effective_cwd: cwd_state.effective_cwd,
            message: Some(opaque.message),
            // The resolver's compound teaching already carries the misc-154
            // "no leg has run" clause; naming the resolved earlier legs would
            // need the failing leg's statement index, which `OpaqueWrite`
            // doesn't carry — out of misc 206's scope.
            skipped_writes: Vec::new(),
        }),
    }
}

/// Effective working directory threaded across a script's commands.
///
/// `cd <path>` segments update [`Self::effective_cwd`] (used for per-root build
/// tool resolution); an unresolvable target (`$VAR`, `$(...)`) flips
/// [`Self::saw_unresolved_cd`] so a later denial can note the stale cwd.
struct CwdState {
    effective_cwd: Option<std::path::PathBuf>,
    saw_unresolved_cd: bool,
}

/// Walk a [`ParsedScript`](parse::ParsedScript)'s pipelines and commands in
/// document order, validating each command position against the allowlist and
/// returning the first [`Denial`] paired with the index of the pipeline
/// (list-level statement) it occurred in — a denial inside a substitution
/// reports the hosting statement's index. The top-level caller uses the index
/// to resolve the writes of the statements that never ran (misc 206).
///
/// Segmentation is the parse's: a `Pipeline` is one list element (separated by
/// `;` / `&&` / `||` / newline / `&`); its `commands` are the pipe stages, so
/// the stage index is the pipe position the `pipeline`-class position-0 guard
/// reads. Command substitutions are recursed here (not via a separate scan), so
/// a redirect or denied command inside `$()` / `` `…` `` / `<(…)` / `>(…)` is
/// still caught. `cwd` threads across commands so a `cd` updates the build-tool
/// resolution for the rest of the walk.
fn check_script(
    script: &parse::ParsedScript,
    rules: &ResolvedCommands,
    cwd: &mut CwdState,
    session: &SessionContext,
) -> Option<(usize, Denial)> {
    for (stmt_idx, pipeline) in script.pipelines.iter().enumerate() {
        for (pipe_pos, command) in pipeline.commands.iter().enumerate() {
            if let Some(denial) = check_parsed_command(command, pipe_pos, rules, cwd, session) {
                return Some((stmt_idx, denial));
            }
        }
    }
    None
}

/// Validate one [`SimpleCommand`](parse::SimpleCommand) at pipe position
/// `pipe_pos`, recursing its substitutions first, then applying the redirect
/// guard and the allowlist. Updates `cwd` on a `cd`. Returns the first
/// [`Denial`].
fn check_parsed_command(
    command: &parse::SimpleCommand,
    pipe_pos: usize,
    rules: &ResolvedCommands,
    cwd: &mut CwdState,
    session: &SessionContext,
) -> Option<Denial> {
    // Recurse substitutions first — a denied command (or a redirect) inside
    // `$()` / `` `…` `` / `<(…)` / `>(…)` is caught regardless of the host
    // command's own name (including a `catenary` host, skipped below). The
    // sub-script's own statement index is dropped: the hosting statement's
    // index (the outer walk's) is the one the misc-206 skipped-writes
    // resolution reads.
    for sub in &command.substitutions {
        if let Some((_, denial)) = check_script(sub, rules, cwd, session) {
            return Some(denial);
        }
    }

    // A command with no command-position word (only assignments / redirects /
    // a `for` header) has nothing to validate at the allowlist.
    let name = command.name.as_deref()?;

    // Catenary's own commands run under the canonical-form matcher (regime 1,
    // `analyze_catenary_command`), not the foreign allowlist. Skip them here so
    // a search chain's foreign segments (e.g. `cd src && catenary grep p`) are
    // still validated without `catenary` itself tripping the denylist.
    if name == "catenary" {
        return None;
    }

    // `git worktree` is denied on the agent surface (misc 151): the sanctioned
    // `catenary worktree` surface owns worktree lifecycle, and a hand-run
    // `git worktree add` inside a repo re-opens the bug-53 nesting hazard.
    // Always denied — regardless of whether `git` is otherwise allowlisted —
    // with a teaching message pointing at the replacement (like `git grep`, but
    // built in rather than config-sourced so the pointer always lands).
    if name == "git" && command.argv.first().map(String::as_str) == Some("worktree") {
        return Some(Denial {
            command: "git worktree".to_string(),
            reason: DenialReason::DeniedSubcommand,
            unresolved_cd: cwd.saw_unresolved_cd,
            effective_cwd: cwd.effective_cwd.clone(),
            message: Some(git_worktree_teaching()),
            skipped_writes: Vec::new(),
        });
    }

    // Subagent branch guard (misc 221): a worktree-anchored subagent must not
    // manipulate branches in a repo OUTSIDE its anchored worktree — the incident
    // was a worker leaving the SHARED checkout on a stray branch. Scoped to
    // subagents only (the lead is explicitly untouched, maintainer ruling
    // 2026-07-23); a lead `session` yields no anchor and this is inert. The
    // targeting is the bug-140 vocabulary: an explicit `git -C <path>` /
    // `--git-dir` / `--work-tree` naming a repo outside the anchor. A bare
    // (anchor-targeted) branch op is allowed — the anchored worktree is where
    // branch work belongs.
    if name == "git"
        && let Some(anchor) = session.subagent_anchor()
        && let Some(target) = git_branch_op_outside_anchor(&command.argv, anchor)
    {
        return Some(Denial {
            command: format!("git {target}"),
            reason: DenialReason::DeniedSubcommand,
            unresolved_cd: cwd.saw_unresolved_cd,
            effective_cwd: cwd.effective_cwd.clone(),
            message: Some(subagent_branch_guard_teaching(anchor)),
            skipped_writes: Vec::new(),
        });
    }

    // Output redirection is no longer denied here: the write resolver
    // (`resolver::resolve_script`, run by `check_command` after this walk)
    // resolves every redirect to its complete target set or denies the line
    // as an opaque write (ws38 ticket 01 — the bug-11 blanket deny flipped to
    // resolve-or-deny; decision 026).

    if let Some((denied, reason)) =
        check_against_allowlist(command, pipe_pos, rules, cwd.effective_cwd.as_deref())
    {
        return Some(Denial {
            command: denied,
            reason,
            unresolved_cd: cwd.saw_unresolved_cd,
            effective_cwd: cwd.effective_cwd.clone(),
            message: None,
            skipped_writes: Vec::new(),
        });
    }

    // Track `cd` to update effective cwd for subsequent commands. The target is
    // the first parsed argument after the command word.
    if name == "cd"
        && let Some(target) = command.argv.first()
    {
        if is_unresolvable_cd_target(target) {
            cwd.saw_unresolved_cd = true;
        }
        cwd.effective_cwd = resolve_cd_target(target, cwd.effective_cwd.as_deref());
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
        crate::paths::home_dir()?
    } else if let Some(rest) = target.strip_prefix("~/") {
        crate::paths::home_dir()?.join(rest)
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
// - `diagnostics` (load-bearing, correlated) must be **bare** — the sole
//   command in the call — so its hook→CLI handoff (ticket 17) consumes fast.
// - `grep`/`glob` (stateless, self-scoping) may `cd`-prefix and `&&`/`;`/`||`
//   chain with allowlisted foreign commands, any count.
//
// Both classes reject substitution-*wrapping* (`$(catenary …)` captures the
// output) and backgrounding (`&`; the harness auto-backgrounds and drops stdout,
// bug 15). The output-ownership pipe/redirect denials are retired (decision
// 025): `grep`/`glob` emit complete, client-owned output, so they may pipe or
// redirect freely; only the handoff-carrying `diagnostics`/lifecycle commands
// stay bare-only, so a pipe or redirect on them is a bare-only violation. The
// matcher only recognizes and classifies; it performs no IO. Foreign commands
// keep the allowlist regime ([`check_command`]).

/// Correlation class of a recognized catenary subcommand (ADR 013/014).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CatenaryClass {
    /// `grep`/`glob` — stateless, self-scoping; may chain/`cd`, any count.
    Search,
    /// `diagnostics` — load-bearing, correlated; bare only.
    Correlated,
    /// `editing start`/`roots`/`primer` — bare lifecycle/management.
    Lifecycle,
}

/// A recognized agent-facing catenary subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    Grep,
    Glob,
    /// `catenary query` — read-only telemetry introspection (maintainer
    /// ruling, misc 149: "pure observability"). Search-class: no handoff, no
    /// tracked-set interaction, output is complete and client-owned.
    Query,
    /// Retired: `catenary sed` came out in ws38 ticket 06 — native `sed -i` is
    /// a resolved, tracked write now. Still recognized so a stray invocation
    /// gets a retirement redirect, not a generic "unknown command".
    Sed,
    /// `catenary diagnostics` — prints diagnostics for the edited files.
    Diagnostics,
    EditingStart,
    /// Retired: `editing stop` was renamed to `diagnostics` (ticket 05). Still
    /// recognized so a stray invocation gets a redirect, not a generic
    /// "unknown command".
    EditingStop,
    /// Bare `catenary roots` (and the kept `roots ls` alias) — lists the
    /// current roots. Bare-only lifecycle.
    Roots,
    /// Retired: `catenary roots add`/`roots rm` were renamed to the top-level
    /// `catenary pin`/`catenary unpin` (misc 146). Still recognized so a stray
    /// invocation gets a redirect naming the new spelling, not a generic
    /// "unknown command".
    RootsAddRm,
    /// `catenary pin <path>` — pin a workspace root. Bare-only lifecycle.
    Pin,
    /// `catenary unpin <path>` — unpin a workspace root. Bare-only lifecycle.
    Unpin,
    /// `catenary claim <root>` — take over a root's durable lock and its
    /// diagnostic debt (root-ownership stage 2). Agent-invocable, bare-only
    /// lifecycle: it mutates the on-disk lock and must run as the sole command.
    Claim,
    Primer,
    /// `catenary commands` — prints the allowed-command surface.
    Commands,
    /// `catenary worktree ls` — registry+sidecar view (misc 151). Search-class:
    /// no handoff, output is complete and client-owned, so it chains and pipes
    /// like `query`.
    WorktreeLs,
    /// `catenary worktree add` — the durable-worktree creation verb (misc 151).
    /// Bare-only lifecycle: it mutates the on-disk worktree set and must run as
    /// the sole command. On clients whose installed hook set registers
    /// `WorktreeCreate` it is denied outright with a dispatch teaching
    /// (misc 177): the hook-driven `isolation: "worktree"` flow creates,
    /// relocates, and anchors the worktree — a hand-run add anchors nothing.
    WorktreeAdd,
    /// `catenary worktree rm` — the durable-worktree removal verb (misc 151)
    /// and, with `land`, the sanctioned cleanup path (`WorktreeRemove` never
    /// fires upstream). Bare-only lifecycle: it mutates the on-disk worktree
    /// set and must run as the sole command.
    WorktreeRm,
    /// `catenary worktree diff` — retired to a transition-period teaching stub
    /// (wf-03): the CLI prints the git-native review/landing flow and exits 2.
    /// Kept recognized (Search-class, as before) so the stub can teach; the
    /// variant deletes with the stub in a later release.
    WorktreeDiff,
    /// `catenary worktree land` — retired to a transition-period teaching stub
    /// (wf-03): the patch engine is gone, `git merge` carries landing (and its
    /// debt transfer) now, and the CLI prints that flow and exits 2. Kept
    /// recognized (bare-only lifecycle, as before) so the stub can teach; the
    /// variant deletes with the stub in a later release.
    WorktreeLand,
}

impl Sub {
    /// Correlation class governing the canonical-form rules.
    const fn class(self) -> CatenaryClass {
        match self {
            Self::Grep | Self::Glob | Self::Query | Self::WorktreeLs | Self::WorktreeDiff => {
                CatenaryClass::Search
            }
            Self::Sed | Self::Diagnostics | Self::EditingStop => CatenaryClass::Correlated,
            Self::EditingStart
            | Self::Roots
            | Self::RootsAddRm
            | Self::Pin
            | Self::Unpin
            | Self::Claim
            | Self::Primer
            | Self::Commands
            | Self::WorktreeAdd
            | Self::WorktreeRm
            | Self::WorktreeLand => CatenaryClass::Lifecycle,
        }
    }

    /// Display form for deny messages (the canonical subcommand words).
    const fn label(self) -> &'static str {
        match self {
            Self::Grep => "grep",
            Self::Glob => "glob",
            Self::Query => "query",
            Self::Sed => "sed",
            Self::Diagnostics => "diagnostics",
            Self::EditingStart => "editing start",
            Self::EditingStop => "editing stop",
            Self::Roots => "roots",
            Self::RootsAddRm => "roots add/rm",
            Self::Pin => "pin",
            Self::Unpin => "unpin",
            Self::Claim => "claim",
            Self::Primer => "primer",
            Self::Commands => "commands",
            Self::WorktreeLs => "worktree ls",
            Self::WorktreeAdd => "worktree add",
            Self::WorktreeRm => "worktree rm",
            Self::WorktreeDiff => "worktree diff",
            Self::WorktreeLand => "worktree land",
        }
    }
}

/// Recognition outcome for the words following `catenary`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recog {
    /// An agent-facing subcommand.
    Agent(Sub),
    /// A global read: a sole `catenary --version`/`-V` or `catenary --help`/`-h`
    /// (subcommand-less — clap handles them globally; admitted only when the
    /// flag is the sole argument, so `catenary --version extra` stays
    /// fail-closed, bug 22 / misc 142), or the `version` subcommand (the same
    /// read plus a stateless daemon-version query, so CLI/daemon staleness is
    /// visible at a glance). A pure, side-effect-free introspection — no
    /// handoff, no tracked-set interaction — so it is admitted.
    GlobalRead,
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
        // `roots add`/`roots rm` retired to `catenary pin`/`catenary unpin` (misc
        // 146): recognized before the bare-word arm so they get a rename redirect.
        // Bare `catenary roots` (and the kept `roots ls` alias) lists the roots.
        (Some("roots"), Some("add" | "rm")) => Recog::Agent(Sub::RootsAddRm),
        (Some("roots"), _) => Recog::Agent(Sub::Roots),
        (Some("pin"), _) => Recog::Agent(Sub::Pin),
        (Some("unpin"), _) => Recog::Agent(Sub::Unpin),
        (Some("claim"), _) => Recog::Agent(Sub::Claim),
        // `worktree ls` is Search-class (pipe-friendly registry view); `worktree
        // add`/`rm` are bare-only lifecycle verbs (misc 151), recognized apart so
        // `add` alone can take the client-keyed dispatch deny (misc 177). Split
        // before the bare-word arms so the two-word forms are matched exactly.
        (Some("worktree"), Some("ls")) => Recog::Agent(Sub::WorktreeLs),
        (Some("worktree"), Some("add")) => Recog::Agent(Sub::WorktreeAdd),
        (Some("worktree"), Some("rm")) => Recog::Agent(Sub::WorktreeRm),
        // `worktree diff`/`worktree land` retired to teaching stubs (wf-03):
        // still recognized in their old classes (Search / bare-only lifecycle)
        // so an invocation reaches the CLI stub, which prints the git-native
        // flow and exits 2. These arms delete with the stubs in a later release.
        (Some("worktree"), Some("diff")) => Recog::Agent(Sub::WorktreeDiff),
        (Some("worktree"), Some("land")) => Recog::Agent(Sub::WorktreeLand),
        (Some("grep"), _) => Recog::Agent(Sub::Grep),
        (Some("glob"), _) => Recog::Agent(Sub::Glob),
        (Some("query"), _) => Recog::Agent(Sub::Query),
        (Some("sed"), _) => Recog::Agent(Sub::Sed),
        (Some("diagnostics"), _) => Recog::Agent(Sub::Diagnostics),
        (Some("primer"), _) => Recog::Agent(Sub::Primer),
        (Some("commands"), _) => Recog::Agent(Sub::Commands),
        (
            Some(
                "hook" | "start" | "stop" | "restart" | "quit" | "debug" | "config" | "doctor"
                | "install" | "update" | "daemon",
            ),
            _,
        ) => Recog::NotAgent,
        // Global reads — pure, side-effect-free introspection (no handoff, no
        // tracked-set interaction), admitted after the subcommand arms and
        // before the fail-closed fallthrough (bug 22 / misc 142):
        //
        // - `catenary version`: a real subcommand (reports the CLI version and
        //   the running daemon's) — the richer version probe, recognized like
        //   the other subcommand arms above.
        // - a subcommand-less `--version`/`-V` or `--help`/`-h`: clap
        //   short-circuits these before any subcommand, so they reach here only
        //   as the *first* token (the subcommand arms above already claimed
        //   `catenary grep --help` via `grep`). Admitted ONLY as the sole
        //   argument (`rest.get(1)` is `None`) — a flag plus an extra arg
        //   (`catenary --version extra`), paired flags, or an unknown flag stay
        //   fail-closed.
        (Some("version"), _) | (Some("--version" | "-V" | "--help" | "-h"), None) => {
            Recog::GlobalRead
        }
        _ => Recog::Unknown,
    }
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
    /// The occurrence is `catenary worktree rm --force` — the dirty-discard
    /// lever, denied client-keyed on `WorktreeCreate` hosts (misc 188). The
    /// flag is read from the parsed argv at scan time, so it is available
    /// alongside `recog` for the early dispatch deny.
    forced_worktree_rm: bool,
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
    /// A bare, canonical `catenary claim <root>` — perform the root-lock takeover
    /// hook-side (identity lives at the hook, root-ownership stage 2), stage the
    /// rendered answer, then allow the command to run (it prints the answer).
    Claim,
    /// A canonical catenary command (`grep`/`glob`/`roots`/`primer`).
    /// `has_foreign` is true when the call also contains foreign segments (a
    /// search chain) whose allowlist must still be checked.
    Allow {
        /// Whether foreign segments are present and need allowlist validation.
        has_foreign: bool,
    },
}

/// What the [`ParsedScript`](parse::ParsedScript) walk found: every `catenary`
/// occurrence plus the structural facts the isolation gate needs.
#[derive(Default)]
struct CatenaryScan {
    /// Every recognized `catenary` occurrence, in document order.
    occs: Vec<CatenaryOcc>,
    /// Top-level command positions with a command word (across all pipelines /
    /// pipe stages, *not* counting substitution-internal ones). The isolation
    /// gate's count of "how many commands the shell runs at this level".
    total_top_level: usize,
    /// A top-level foreign (non-`catenary`) command position is present.
    has_foreign_segment: bool,
    /// A foreign command position appears inside an arg-substitution
    /// (`catenary grep "$(foo)"`) — permitted by hygiene but still
    /// allowlist-checked (regime 2). Flagged so the caller runs that check even
    /// with no top-level foreign segment.
    inner_foreign_substitution: bool,
    /// Any top-level segment is a compound (`for`/`while`/`if`/`{`/subshell).
    /// A correlated catenary command inside a compound is *wrapped*, so it can
    /// never be the canonical sole command — the under-counting hazard the
    /// isolation gate must catch structurally (decision 020 §5/§7).
    compound_present: bool,
}

/// Recognize, classify, and validate any `catenary` command in a shell call.
///
/// Regime 1 of ADR 013, reading the faithful [`ParsedScript`](parse::parse)
/// rather than the ad-hoc scanners (ticket 04). Pure — no IO. See
/// [`CatenaryAction`].
///
/// The two regimes are split by correlation class: `grep`/`glob` carry no
/// hook→IPC handoff, so they may be `cd`-prefixed and `&&`/`;`/`||`-chained
/// with allowlisted foreign commands, any count. The correlated, load-bearing
/// `catenary diagnostics` takes the handoff and must therefore be the *sole*
/// command of the whole script: a non-isolated invocation (`sleep 100; catenary
/// diagnostics`, or a `for`-loop body) would wedge the daemon (decision 020 §5).
/// That gate is **structural** — it counts the parse's command positions and
/// inspects its compound flag, never a substring scan, so an under-counted
/// separator can never mistake a chained command for isolated. Both classes
/// reject substitution-wrapping and backgrounding; the output-ownership
/// pipe/redirect denials are retired for the handoff-free `grep`/`glob`, which
/// emit complete, client-owned output (decision 025), while a pipe or redirect
/// on a handoff command is a bare-only violation. The retired `catenary sed` /
/// `editing stop` tokens get a teaching redirect; unrecognized or non-agent
/// subcommands are denied.
///
/// `client` is the declared client identity (the `--format=<client>` the hook
/// definition carries — declared, never sniffed, exactly like the primer's
/// client keying). When the declared client's installed hook set registers
/// `WorktreeCreate`, `catenary worktree add` is denied in any form with the
/// dispatch teaching (misc 177): the sanctioned flow is the Agent/Task tool's
/// `isolation: "worktree"`, whose hook creates, relocates, and anchors the
/// worktree — a hand-run add anchors nothing. `None` (no declared client — the
/// daemon-side boundary classifier, tests) keeps `worktree add` a plain
/// bare-only lifecycle verb.
#[must_use]
pub fn analyze_catenary_command(cmd: &str, client: Option<HostFormat>) -> CatenaryAction {
    // The parse strips heredoc bodies, `#` comments, and `\`-newline joins, and
    // segments on the real list/pipe operators quote-faithfully — so a `catenary`
    // word inside a quoted argument or comment never reaches command position.
    let script = parse::parse(cmd);
    let scan = scan_catenary(&script);

    if scan.occs.is_empty() {
        return CatenaryAction::NotCatenary;
    }

    // `catenary editing stop` is retired — renamed to `catenary diagnostics`
    // (ticket 05). Catch it in any form, before the output-ownership and
    // bare-only denials, so the agent always learns the new name rather than a
    // generic output-ownership complaint.
    if scan
        .occs
        .iter()
        .any(|o| matches!(o.recog, Recog::Agent(Sub::EditingStop)))
    {
        return CatenaryAction::Deny(editing_stop_retired_denial());
    }

    // `catenary sed` is retired — native `sed -i` is a resolved, tracked write
    // now (ws38 ticket 06). Catch it in any form, before the output-ownership
    // and bare-only denials, so the agent learns the native form rather than a
    // generic complaint.
    if scan
        .occs
        .iter()
        .any(|o| matches!(o.recog, Recog::Agent(Sub::Sed)))
    {
        return CatenaryAction::Deny(sed_retired_denial());
    }

    // `catenary roots add`/`roots rm` are retired — renamed to the top-level
    // `catenary pin`/`catenary unpin` (misc 146). Catch in any form, before the
    // output-ownership and bare-only denials, so the agent learns the new
    // spelling rather than a generic complaint.
    if scan
        .occs
        .iter()
        .any(|o| matches!(o.recog, Recog::Agent(Sub::RootsAddRm)))
    {
        return CatenaryAction::Deny(roots_add_rm_retired_denial());
    }

    // Agent-side `catenary worktree add` is structurally unavailable on clients
    // whose installed hook set registers `WorktreeCreate` (misc 177): the
    // sanctioned dispatch flow is the Agent/Task tool's `isolation: "worktree"`,
    // whose hook creates, relocates, and anchors the worktree — a hand-run add
    // anchors nothing (the misc-172 mislocation class). Catch it in any form,
    // before the output-ownership and bare-only denials, so the agent always
    // learns the dispatch flow rather than a generic complaint. Operator
    // hand-runs are untouched — humans at terminals are unfiltered.
    if crate::cli::teaching::hook_set_has_worktree_create(client)
        && scan
            .occs
            .iter()
            .any(|o| matches!(o.recog, Recog::Agent(Sub::WorktreeAdd)))
    {
        return CatenaryAction::Deny(worktree_add_dispatch_denial());
    }

    // Agent-side `catenary worktree rm --force` is the dirty-discard lever —
    // auto-discarding uncommitted work is the fatal sin on this surface, so it
    // is denied client-keyed on the same `WorktreeCreate` hosts (misc 188).
    // Bare `worktree rm` stays allowed (it refuses dirty worktrees itself,
    // misc 158); only the explicit `--force` is bounced, in any form, before
    // the output-ownership and bare-only denials, so the agent always learns
    // the worktree lifecycle rather than a generic complaint. Operator
    // hand-runs are untouched — humans at terminals are unfiltered.
    if crate::cli::teaching::hook_set_has_worktree_create(client)
        && scan
            .occs
            .iter()
            .any(|o| matches!(o.recog, Recog::Agent(Sub::WorktreeRm)) && o.forced_worktree_rm)
    {
        return CatenaryAction::Deny(worktree_rm_force_dispatch_denial());
    }

    // First occurrence with a per-command problem wins (document order).
    for occ in &scan.occs {
        if let Some(msg) = catenary_occ_denial(occ) {
            return CatenaryAction::Deny(msg);
        }
    }

    // Every occurrence is a clean, agent-invocable command.
    let subs: Vec<Sub> = scan
        .occs
        .iter()
        .filter_map(|o| match o.recog {
            Recog::Agent(s) => Some(s),
            Recog::GlobalRead | Recog::NotAgent | Recog::Unknown => None,
        })
        .collect();

    let has_foreign = scan.has_foreign_segment || scan.inner_foreign_substitution;

    // Isolation gate (decision 020 §7.1). An occurrence that takes the hook→IPC
    // handoff — `diagnostics`, `editing start`, and the bare lifecycle commands
    // (`roots`/`primer`/`commands`) — must be the *sole* command of the whole
    // script: a non-isolated invocation wedges the daemon. `grep`/`glob` carry
    // no handoff, so they chain freely.
    if scan.occs.iter().any(occ_needs_isolation) {
        // Canonical only when the script is exactly *one* top-level command — no
        // chaining (one command position), no compound wrapper, and the lone
        // catenary occurrence (a wrapped one is caught above). The check is count-
        // and structure-based on the parse, never a substring scan, so an
        // under-counted separator can't slip a chained command past as isolated
        // (decision 020 §5).
        if scan.total_top_level != 1 || scan.occs.len() != 1 || scan.compound_present {
            return CatenaryAction::Deny(bare_only_denial(&subs));
        }
        return match subs.first() {
            Some(Sub::EditingStart) => CatenaryAction::EditingStart,
            Some(Sub::Diagnostics) => CatenaryAction::Diagnostics,
            Some(Sub::Claim) => CatenaryAction::Claim,
            // A bare lifecycle command (`roots`/`primer`/`commands`).
            _ => CatenaryAction::Allow { has_foreign },
        };
    }

    // No handoff anywhere — `grep`/`glob` chain freely.
    CatenaryAction::Allow { has_foreign }
}

/// Whether a clean catenary occurrence takes the hook→IPC handoff and so must be
/// the sole command of the script (the isolation gate, decision 020 §7.1):
/// `diagnostics`, `editing start`, and the bare lifecycle commands
/// (`roots`/`primer`/`commands`). A `grep`/`glob` search carries no handoff and
/// chains freely. `NotAgent`/`Unknown` and the retired `sed`/`editing stop`
/// redirects were already handled before this runs.
const fn occ_needs_isolation(occ: &CatenaryOcc) -> bool {
    match occ.recog {
        // diagnostics / editing start / roots / primer / commands take the
        // handoff (or are bare-only lifecycle). The retired `editing stop` /
        // `sed` redirects are caught before this runs; grouped here for
        // exhaustiveness.
        Recog::Agent(
            Sub::Diagnostics
            | Sub::EditingStart
            | Sub::EditingStop
            | Sub::Sed
            | Sub::Roots
            | Sub::RootsAddRm
            | Sub::Pin
            | Sub::Unpin
            | Sub::Claim
            | Sub::Primer
            | Sub::Commands
            | Sub::WorktreeAdd
            | Sub::WorktreeRm
            | Sub::WorktreeLand,
        ) => true,
        // search (grep/glob/query/worktree ls/diff), the subcommand-less global
        // read, and the already-denied non-agent/unknown forms carry no handoff.
        Recog::Agent(Sub::Grep | Sub::Glob | Sub::Query | Sub::WorktreeLs | Sub::WorktreeDiff)
        | Recog::GlobalRead
        | Recog::NotAgent
        | Recog::Unknown => false,
    }
}

/// Walk a [`ParsedScript`](parse::ParsedScript), collecting every `catenary`
/// occurrence (with its output-ownership context) and the structural facts the
/// isolation gate reads. Recurses into command substitutions, where a `catenary`
/// command is *wrapped* (`$(catenary …)` captures its output) and a foreign
/// command is flagged for the regime-2 allowlist.
fn scan_catenary(script: &parse::ParsedScript) -> CatenaryScan {
    let mut scan = CatenaryScan::default();
    scan_catenary_into(script, &mut scan);
    scan
}

/// Walk the top-level pipelines of the script the gate dispatches on, recording
/// each `catenary` / foreign command position, the compound flag, and the
/// isolation gate's command count. Command substitutions are walked by
/// [`scan_substitution`] (their commands are not separate top-level segments,
/// but their `catenary` occurrences are *wrapped* and their foreign commands are
/// allowlist-flagged).
fn scan_catenary_into(script: &parse::ParsedScript, scan: &mut CatenaryScan) {
    for pipeline in &script.pipelines {
        let stage_count = pipeline.commands.len();
        for (pipe_pos, command) in pipeline.commands.iter().enumerate() {
            // Recurse into this command's substitutions first: a wrapped
            // `catenary` occurrence or an inner foreign command is recorded
            // regardless of the host command's own name.
            for sub in &command.substitutions {
                scan_substitution(sub, scan);
            }

            if command.is_compound {
                scan.compound_present = true;
            }

            let Some(name) = command.name.as_deref() else {
                continue;
            };

            // A named command position is one command the shell runs at this
            // level — the isolation gate's count, catenary or foreign.
            scan.total_top_level += 1;

            if name == "catenary" {
                let recog = recognize_catenary_argv(&command.argv);
                let piped_out = if pipe_pos + 1 < stage_count {
                    pipeline.commands[pipe_pos + 1].name.clone()
                } else {
                    None
                };
                scan.occs.push(CatenaryOcc {
                    recog,
                    piped_in: pipe_pos > 0,
                    piped_out,
                    redirected: command.redirects.iter().any(redirect_writes_file),
                    backgrounded: pipeline.backgrounded,
                    wrapped: false,
                    forced_worktree_rm: worktree_rm_is_forced(&command.argv),
                });
            } else {
                scan.has_foreign_segment = true;
            }
        }
    }
}

/// Record a substitution's contents: a `catenary` command in it is *wrapped*
/// (its output captured — denied), a foreign command in it is flagged for the
/// regime-2 allowlist. Recurses through nested substitutions and compounds.
fn scan_substitution(sub: &parse::ParsedScript, scan: &mut CatenaryScan) {
    for pipeline in &sub.pipelines {
        for command in &pipeline.commands {
            for inner in &command.substitutions {
                scan_substitution(inner, scan);
            }
            let Some(name) = command.name.as_deref() else {
                continue;
            };
            if name == "catenary" {
                scan.occs.push(CatenaryOcc {
                    recog: recognize_catenary_argv(&command.argv),
                    piped_in: false,
                    piped_out: None,
                    redirected: false,
                    backgrounded: false,
                    wrapped: true,
                    forced_worktree_rm: worktree_rm_is_forced(&command.argv),
                });
            } else {
                scan.inner_foreign_substitution = true;
            }
        }
    }
}

/// Classify the words after a `catenary` command word (its parsed `argv`).
fn recognize_catenary_argv(argv: &[String]) -> Recog {
    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
    recognize_catenary_sub(&refs)
}

/// Whether the argv is a `catenary worktree rm` invocation carrying `--force`
/// (its long-only flag — `catenary worktree rm <path> [--force]`, misc 166).
///
/// Read from the parsed argv so the client-keyed dispatch deny (misc 188) can
/// distinguish the dirty-discard lever from a bare clean removal without
/// touching `Recog`/`Sub`, which stay flag-blind. Any token position is
/// accepted (`--force` before or after the path); `--force=…` value forms and
/// the bare-only `rm` stay off the lever.
fn worktree_rm_is_forced(argv: &[String]) -> bool {
    matches!(
        (
            argv.first().map(String::as_str),
            argv.get(1).map(String::as_str)
        ),
        (Some("worktree"), Some("rm"))
    ) && argv.iter().any(|a| a == "--force")
}

/// Per-occurrence deny reason in priority order, or `None` if the occurrence is
/// a clean, agent-invocable command.
fn catenary_occ_denial(occ: &CatenaryOcc) -> Option<String> {
    let sub = match occ.recog {
        Recog::Unknown => return Some(unknown_subcommand_denial()),
        Recog::NotAgent => return Some(not_agent_invocable_denial()),
        // A subcommand-less global read (`--version`/`--help`) is a pure read —
        // admit it with no output-ownership concern (bug 22).
        Recog::GlobalRead => return None,
        Recog::Agent(s) => s,
    };
    if occ.wrapped {
        return Some(substitution_denial(sub));
    }
    // `… | catenary X`: Search commands read stdin (ticket 04), so a downstream
    // pipe is valid for them (`stdin_denial` returns `None`); the no-stdin
    // classes still deny. This is the retired bug-19 / ADR-013 post-pipe guard.
    if occ.piped_in
        && let Some(msg) = stdin_denial(sub)
    {
        return Some(msg);
    }
    // Output-ownership pipe/redirect denials are retired for the handoff-free
    // commands (decision 025): `grep`/`glob` emit complete, client-owned
    // output, so piping or redirecting them is as valid as piping `grep`
    // itself. Only the handoff-carrying commands (`diagnostics`, the bare
    // lifecycle commands) stay bare-only, so a pipe-out or a file redirect on
    // them is a bare-only violation.
    if occ_needs_isolation(occ) {
        if let Some(down) = &occ.piped_out {
            return Some(out_pipe_denial(sub, down));
        }
        if occ.redirected {
            return Some(redirect_denial(sub));
        }
    }
    // Backgrounding is denied on the bug-15 ground (the harness auto-backgrounds
    // and stdout is dropped), not output ownership.
    if occ.backgrounded {
        return Some(background_denial(sub));
    }
    None
}

/// The recognized agent-facing command surface, for "unknown subcommand" denials.
const CATENARY_SURFACE: &str = "Available: `grep`, `glob`, `query`, `diagnostics`, \
     `editing start`, `pin`, `unpin`, `roots`, `worktree ls/add/rm`, \
     `commands`, `primer`, `version`. Run `catenary primer` for the workflow.";

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

/// `… | catenary X` — the pipe-in verdict for a downstream `catenary` command.
///
/// The Search class (`grep`/`glob`) reads stdin (ticket 04), so a downstream
/// pipe position is a valid invocation — `None`. This retires the bug-19 /
/// ADR-013 post-pipe guard, whose rationale ("catenary grep ignores stdin, so
/// the pipe is a no-op") evaporated once stdin landed. The no-stdin classes
/// (correlated `diagnostics` and the bare lifecycle commands) still deny with a
/// class-appropriate message. The retired `sed`/`editing stop` redirects are
/// handled before this runs; their arm here only satisfies exhaustiveness.
fn stdin_denial(sub: Sub) -> Option<String> {
    match sub {
        // Search reads stdin now — a downstream pipe is valid, not an error.
        Sub::Grep | Sub::Glob => None,
        // Search-class but stdin-less: telemetry comes from the daemon, so a
        // pipe INTO query / worktree ls is a no-op; their output still pipes
        // freely.
        Sub::Query => {
            Some("`catenary query` takes no stdin — invoke it first in the pipeline.".to_string())
        }
        Sub::WorktreeLs => Some(
            "`catenary worktree ls` takes no stdin — invoke it first in the pipeline.".to_string(),
        ),
        // `worktree diff` (a retired teaching stub, wf-03) takes no stdin: a
        // pipe INTO it is a no-op; its teaching output pipes freely.
        Sub::WorktreeDiff => Some(
            "`catenary worktree diff` takes no stdin — invoke it first in the pipeline."
                .to_string(),
        ),
        Sub::Diagnostics => {
            Some("`catenary diagnostics` takes no input — run it bare.".to_string())
        }
        Sub::Sed
        | Sub::EditingStart
        | Sub::EditingStop
        | Sub::Roots
        | Sub::RootsAddRm
        | Sub::Pin
        | Sub::Unpin
        | Sub::Claim
        | Sub::Primer
        | Sub::Commands
        | Sub::WorktreeAdd
        | Sub::WorktreeRm
        | Sub::WorktreeLand => Some(format!(
            "`catenary {}` takes no stdin — run it bare.",
            sub.label()
        )),
    }
}

/// `catenary X | downstream` for a handoff-carrying (bare-only) command.
///
/// Only reached for commands that take a hook→IPC handoff (`diagnostics`, the
/// bare lifecycle commands) — the caller gates on [`occ_needs_isolation`].
/// `grep`/`glob` emit complete, client-owned output (decision 025) and pipe
/// freely, so they never reach here. Teaches the bare form / class split, not
/// the retired volume model.
fn out_pipe_denial(sub: Sub, downstream: &str) -> String {
    format!(
        "`catenary {}` takes a daemon handoff and must run bare — don't pipe it \
         into `{downstream}`. Run it as its own command and read the output \
         directly.",
        sub.label()
    )
}

/// `catenary X > file` for a handoff-carrying (bare-only) command.
///
/// Only reached for handoff-carrying commands — the caller gates on
/// [`occ_needs_isolation`]. `grep`/`glob` emit complete, client-owned output
/// (decision 025) and may redirect freely (`catenary grep p > hits.txt` is
/// permitted), so they never reach here. Teaches the bare form, not the retired
/// volume model.
fn redirect_denial(sub: Sub) -> String {
    format!(
        "`catenary {}` takes a daemon handoff and must run bare — don't redirect \
         it to a file. Run it as its own command and read the output directly.",
        sub.label()
    )
}

/// `catenary X &` — backgrounding is denied on the bug-15 ground, not output
/// ownership: the harness auto-backgrounds the command and its stdout is dropped.
fn background_denial(sub: Sub) -> String {
    format!(
        "Don't background `catenary {}` with `&` — the harness auto-backgrounds it \
         and its stdout is dropped, so you'd get no output. Run it in the \
         foreground.",
        sub.label()
    )
}

/// Bare-only violation for a correlated/lifecycle command sharing the call.
///
/// Names the isolation-needing command — the first `Correlated`/`Lifecycle`
/// sub, the one `occ_needs_isolation` gates on — not `subs.first()`, which in a
/// mixed chain like `catenary grep x && catenary diagnostics` would be the
/// freely-chaining `grep`. Falls back to `diagnostics` if none resolves.
fn bare_only_denial(subs: &[Sub]) -> String {
    let label = subs
        .iter()
        .find(|s| {
            matches!(
                s.class(),
                CatenaryClass::Correlated | CatenaryClass::Lifecycle
            )
        })
        .map_or("diagnostics", |s| s.label());
    format!(
        "Run `catenary {label}` as its own command — `diagnostics` and the \
         editing-lifecycle commands take a daemon handoff and must be the SOLE \
         command (no `cd` prefix, no `&&`/`;`/`||` chain, not combined with another \
         command). It must reach the daemon promptly to attribute correctly."
    )
}

/// The agent-surface denial for `git worktree` (misc 151).
///
/// The sanctioned `catenary worktree` surface owns worktree lifecycle. A
/// hand-run `git worktree add` inside a repo re-opens the bug-53 nesting hazard,
/// so every `git worktree` subcommand is denied here, pointing at the
/// replacement (the deny is agent-side only; the human/daemon still uses raw
/// git).
fn git_worktree_teaching() -> String {
    "`git worktree` isn't allowed — use `catenary worktree` instead: `catenary \
     worktree ls` to list, `catenary worktree add <branch> [path]` to create a \
     durable checkout, `catenary worktree rm <path>` to remove one. To dispatch \
     isolated agent work, use the Agent/Task tool's `isolation: \"worktree\"` — \
     never a hand-run add (misc 177). Catenary owns worktree placement and \
     disposal (misc 144/151)."
        .to_string()
}

/// The git globals that redirect a command at a repo other than the cwd's, with
/// their value carried in the FOLLOWING token (`git -C <path> …`,
/// `git --git-dir <path> …`, `git --work-tree <path> …`). Extracting the value
/// is what lets the subagent branch guard (misc 221) tell whether a branch
/// operation targets a repo OUTSIDE the anchored worktree. Mirrors the write
/// resolver / tier `split_git` option-skip, but captures the target rather than
/// merely stepping over it.
const GIT_TARGET_GLOBALS: &[&str] = &["-C", "--git-dir", "--work-tree"];

/// The `git` global options that carry a value in the following token but do
/// **not** re-target the repo (`-c key=val`, `--namespace ns`, …). Stepped over
/// so the subcommand is read from the right position, exactly like
/// [`GIT_TARGET_GLOBALS`], but their values are irrelevant to the guard.
const GIT_VALUE_GLOBALS: &[&str] = &["-c", "--namespace", "--exec-path", "--super-prefix"];

/// The branch-manipulating `git` subcommand of a subagent command that targets a
/// repo OUTSIDE its `anchor`, or `None` when the command is not such an
/// operation (misc 221).
///
/// Walks `argv` as globals → subcommand → subcommand-args, capturing any
/// `-C`/`--git-dir`/`--work-tree` target along the way (the bug-140 flag-aware
/// vocabulary; value-carrying non-target globals are stepped over so the
/// subcommand is read from the right position). The guard fires only when BOTH
/// hold:
///
/// 1. an explicit target global names a repo whose resolved path lies outside
///    `anchor` — a bare (anchor-targeted) command carries no external target and
///    is allowed, since the anchored worktree is where branch work belongs; and
/// 2. the subcommand is branch-manipulating: `switch` (always), `checkout` with
///    `-b`/`-B` or a branch-moving bare form, or `branch` in a create / delete /
///    move / copy form.
///
/// Returns the matched subcommand token (`switch` / `checkout` / `branch`) for
/// the `"git {token}"` denial form. Deliberately conservative: with no external
/// target the guard never fires, so an anchored-repo branch op is never
/// false-denied.
fn git_branch_op_outside_anchor(argv: &[String], anchor: &std::path::Path) -> Option<&'static str> {
    // Phase 1 — walk the globals, capturing a target and locating the subcommand.
    let mut target: Option<&str> = None;
    let mut i = 0;
    let sub_idx = loop {
        let a = argv.get(i)?.as_str();
        if !a.starts_with('-') {
            break i; // first positional — the subcommand
        }
        // `--opt=value` target global (`-C` is short-only, so the `=` forms are
        // `--git-dir=…` / `--work-tree=…`); carries its own value.
        if let Some((flag, value)) = a.split_once('=')
            && GIT_TARGET_GLOBALS.contains(&flag)
        {
            target = Some(value);
            i += 1;
            continue;
        }
        // Separated-value target global: the value is the following token.
        if GIT_TARGET_GLOBALS.contains(&a) {
            target = argv.get(i + 1).map(String::as_str);
            i += 2;
            continue;
        }
        // Non-target value-carrying globals consume the following token; every
        // other option (`-p`, `--paginate`, a `--opt=value` non-target) consumes
        // one — the same fail-safe step the tier/resolver split uses.
        if GIT_VALUE_GLOBALS.contains(&a) {
            i += 2;
        } else {
            i += 1;
        }
    };

    // No external target → not in scope for the guard (the anchored worktree is
    // the sanctioned place for branch work). An unresolvable target (`$VAR`,
    // command substitution) can't be established as outside — fail open, matching
    // the `cd`-target and skipped-write conservatism elsewhere in this file.
    let target = target?;
    if !target_is_outside_anchor(target, anchor) {
        return None;
    }

    // Phase 2 — classify the subcommand as branch-manipulating.
    let sub = argv.get(sub_idx)?.as_str();
    let rest = &argv[sub_idx + 1..];
    match sub {
        // A branch switch is always a branch operation.
        "switch" => Some("switch"),
        // `checkout -b`/`-B` creates+switches; a bare `git checkout <ref>` moves
        // HEAD to a branch. The pathspec-restore forms (`checkout -- <path>`,
        // `checkout <ref> -- <path>`) touch files, not the branch, so they are
        // NOT branch-manipulating.
        "checkout" if checkout_is_branch_move(rest) => Some("checkout"),
        // `git branch <name>` creates; `-d`/`-D`/`--delete`, `-m`/`-M`/`--move`,
        // `-c`/`-C`/`--copy` delete/move/copy. A read-only listing
        // (`git branch`, `git branch --list`, `-a`/`-r`/`-v`) manipulates
        // nothing.
        "branch" if branch_is_create_delete_move(rest) => Some("branch"),
        _ => None,
    }
}

/// Whether a `git checkout` tail (the tokens after `checkout`) is a
/// branch-moving form rather than a pathspec restore (misc 221).
///
/// Branch-moving: `-b`/`-B` (create+switch) anywhere in the tail, or a bare
/// `checkout <ref>` with no `--` pathspec separator. A `--` separator (or the
/// detach-only `git checkout` with no operand) reads as a file restore /
/// no-branch-move — not the guard's target. Conservative in the guard's favor:
/// an ambiguous `checkout <x>` with no `--` is treated as a branch move (the
/// incident's shape), and since the whole guard is already gated on an external
/// `-C` target, this can only deny a cross-repo checkout.
fn checkout_is_branch_move(rest: &[String]) -> bool {
    // `-b`/`-B` — explicit branch creation+switch.
    if rest
        .iter()
        .any(|a| a == "-b" || a == "-B" || a.starts_with("-b") || a.starts_with("-B"))
    {
        return true;
    }
    // A `--` pathspec separator marks a file restore, not a branch move.
    if rest.iter().any(|a| a == "--") {
        return false;
    }
    // A bare positional operand is the branch/ref to move to.
    rest.iter().any(|a| !a.starts_with('-'))
}

/// Whether a `git branch` tail is a create / delete / move / copy form rather
/// than a read-only listing (misc 221).
///
/// Mutating: a delete/move/copy flag (`-d`/`-D`/`--delete`, `-m`/`-M`/`--move`,
/// `-c`/`-C`/`--copy`) or a bare positional operand (`git branch <name>` creates
/// it). Read-only listing (`git branch`, `--list`, `-a`/`-r`/`-v`/`--contains …`)
/// carries no operand and no mutating flag.
fn branch_is_create_delete_move(rest: &[String]) -> bool {
    const MUTATING_FLAGS: &[&str] = &[
        "-d", "-D", "--delete", "-m", "-M", "--move", "-c", "-C", "--copy",
    ];
    if rest
        .iter()
        .any(|a| MUTATING_FLAGS.contains(&a.split_once('=').map_or(a.as_str(), |(f, _)| f)))
    {
        return true;
    }
    // A bare positional (the new branch name) means a create.
    rest.iter().any(|a| !a.starts_with('-'))
}

/// Whether a `git -C`/`--git-dir`/`--work-tree` target resolves to a location
/// OUTSIDE `anchor` (misc 221).
///
/// The target is resolved relative to the anchor (a relative `-C ../shared`
/// escapes it), then compared by path prefix. A target that resolves at or under
/// the anchor is inside (allowed); anything else is outside. An unresolvable /
/// absolute-elsewhere target is outside. `.git`-suffixed targets (a bare
/// `--git-dir /repo/.git`) compare by their parent-agnostic prefix — a `.git`
/// dir under the anchor is still inside.
fn target_is_outside_anchor(target: &str, anchor: &std::path::Path) -> bool {
    // Resolve the target: absolute stays as-is, relative joins the anchor (the
    // subagent's cwd), then normalize `.`/`..` lexically (no filesystem touch,
    // so a not-yet-existing path still resolves — mirrors `resolve_cd_target`).
    let target_path = std::path::Path::new(target);
    let resolved = if target_path.is_absolute() {
        normalize_path(target_path)
    } else {
        normalize_path(&anchor.join(target_path))
    };
    let anchor_norm = normalize_path(anchor);
    // Inside iff the resolved target is the anchor or a descendant of it.
    !resolved.starts_with(&anchor_norm)
}

/// The teaching message for a subagent branch-guard denial (misc 221).
///
/// Names the anchored worktree as where branch work belongs — the worker's
/// deliverable is its worktree branch — mirroring the `git worktree` denial's
/// shape (a built-in, always-lands pointer). The incident this closes: a worker
/// left the SHARED checkout on a stray branch.
fn subagent_branch_guard_teaching(anchor: &std::path::Path) -> String {
    format!(
        "Branch work belongs in your anchored worktree ({}), not another repo — a \
         subagent must not create, switch, delete, or move branches in a checkout \
         outside its worktree (the incident: a worker left a shared checkout on a \
         stray branch). Your deliverable is your worktree's own branch: commit \
         there, and let the lead land it. Drop the `-C`/`--git-dir`/`--work-tree` \
         target and run branch commands inside your worktree.",
        anchor.display()
    )
}

/// Client-keyed teaching denial for agent-side `catenary worktree add` on
/// clients whose installed hook set registers `WorktreeCreate` (misc 177).
///
/// On such hosts the sanctioned dispatch flow is the Agent/Task tool's
/// `isolation: "worktree"` — the `WorktreeCreate` hook creates the worktree
/// itself, relocates it outside the repo, and anchors the subagent's workspace
/// there. A hand-run add skips that anchoring: the subagent stays pinned to the
/// main tree and its file access prompts against the wrong workspace (the
/// misc-172 mislocation class). The teaching mirrors the primer's "Dispatching
/// isolated work" section. Humans at terminals are unfiltered, so operator
/// hand-runs are untouched.
fn worktree_add_dispatch_denial() -> String {
    "Don't hand-run `catenary worktree add` to dispatch agent work — dispatch \
     with the Agent/Task tool's `isolation: \"worktree\"` instead: Catenary's \
     WorktreeCreate hook creates the worktree itself, relocates it outside the \
     repo, and anchors the subagent's workspace there. A hand-run add skips \
     that anchoring — the agent stays pinned to the main tree and its file \
     access prompts against the wrong workspace. Land finished work with git: \
     commit in the worktree's branch, review with `git diff main...<branch>`, \
     `git merge --squash <branch>` in the owning repo, commit, then \
     `catenary worktree rm <path>`."
        .to_string()
}

/// Client-keyed teaching denial for agent-side `catenary worktree rm --force`
/// on clients whose installed hook set registers `WorktreeCreate` (misc 188).
///
/// `--force` discards a worktree *with* uncommitted work — the fatal sin on
/// this surface, whose standing doctrine is that auto-discarding dirty work is
/// never the agent's call. Bare `catenary worktree rm` stays available (it
/// refuses dirty worktrees itself, misc 158), so the clean-disposal path is
/// untouched; only the explicit lever is bounced. The teaching names the
/// worktree lifecycle so the agent hands the review back rather than reaching
/// for the discard. Humans at terminals are unfiltered, so operator hand-runs
/// keep `--force`.
fn worktree_rm_force_dispatch_denial() -> String {
    "Don't hand-run `catenary worktree rm --force` — discarding a worktree with \
     uncommitted work is the maintainer's lever, not the agent's. On a \
     WorktreeCreate host the worktree lifecycle is git-native: commit in the \
     worktree's branch, review with `git diff main...<branch>`, keep with \
     `git merge --squash <branch>` (then commit), dispose clean with \
     `catenary worktree rm` (bare `rm` refuses a dirty worktree on its own). \
     Surface the dirty worktree for review — don't force-drop the work."
        .to_string()
}

/// Redirect for the retired `catenary editing stop` — renamed to
/// `catenary diagnostics` (ticket 05).
fn editing_stop_retired_denial() -> String {
    "`catenary editing stop` is now `catenary diagnostics` — run `catenary \
     diagnostics` to print diagnostics for your edits."
        .to_string()
}

/// Teaching redirect for the retired `catenary sed` — native `sed -i` is a
/// resolved, tracked write now (ws38 ticket 06, decision 026 §4).
fn sed_retired_denial() -> String {
    "`catenary sed` is retired — native `sed -i 's/a/b/' <files>` is tracked \
     (write-resolution). Preview with plain `sed 's/a/b/' <file>` to stdout and \
     review with `git diff`."
        .to_string()
}

/// Teaching redirect for the retired `catenary roots add`/`roots rm` — renamed
/// to the top-level `catenary pin`/`catenary unpin` (misc 146). Coverage is
/// automatic, so root management is not the agent's job; the redirect names the
/// new spelling rather than a generic "unknown command".
fn roots_add_rm_retired_denial() -> String {
    "`catenary roots add`/`roots rm` are retired — use `catenary pin <path>` to \
     pin a workspace root and `catenary unpin <path>` to unpin one. Bare \
     `catenary roots` lists the current roots."
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
        // DisallowedForm denials early-return in `format_denial` with the
        // config-sourced message naming the permitted forms; this arm only
        // satisfies exhaustiveness.
        DenialReason::DisallowedForm => {
            format!("`{denied_cmd}` isn't allowed in this invocation form.")
        }
        // OpaqueWrite denials early-return in the callers with the resolver's
        // teaching message; this arm only satisfies exhaustiveness.
        DenialReason::OpaqueWrite => {
            format!("`{denied_cmd}` writes through a form the hook can't resolve.")
        }
    }
}

/// One-line pointer to `catenary commands`, where the full allow / pipeline /
/// deny surface lives. Every denial carries this pointer instead of dumping the
/// whole surface inline (decision 023).
const SURFACE_POINTER: &str = "Run `catenary commands` for the allowed command surface.";

/// Cap on the skipped-write targets a denial names inline; the remainder is a
/// count, so a pathological compound can't balloon the teaching (misc 206).
const SKIPPED_WRITES_CAP: usize = 3;

/// The misc-206 honesty line: a denied compound never ran its earlier write
/// legs, so name the writes that never happened — otherwise the agent follows
/// the redirect teaching and reads a stale file as fresh output (bug 117).
/// `None` when there is nothing to name (single-statement denial, denial in
/// the first statement, or no resolved earlier writes) — silence is correct
/// there.
fn format_skipped_writes_note(skipped: &[std::path::PathBuf]) -> Option<String> {
    if skipped.is_empty() {
        return None;
    }
    let mut list = skipped
        .iter()
        .take(SKIPPED_WRITES_CAP)
        .map(|p| format!("`{}`", p.display()))
        .collect::<Vec<_>>()
        .join(", ");
    let rest = skipped.len().saturating_sub(SKIPPED_WRITES_CAP);
    if rest > 0 {
        list = format!("{list}, and {rest} more");
    }
    let noun = if skipped.len() == 1 {
        "the write"
    } else {
        "the writes"
    };
    Some(format!(
        "Note: nothing in this command ran, including {noun} to {list}."
    ))
}

/// Render the `allow_flags` (form-lever) denial: a config-sourced, misc-119
/// voice message that names the permitted invocation forms. The forms are
/// sorted for a deterministic listing.
fn format_disallowed_form_denial(cmd: &str, forms: &std::collections::HashSet<String>) -> String {
    let mut sorted: Vec<&str> = forms.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    let list = sorted
        .iter()
        .map(|f| format!("`{f}`"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "`{cmd}` is limited by the Catenary configuration to these invocation forms: {list}. \
         This invocation matches none of them — re-run `{cmd}` in one of the permitted forms."
    )
}

/// Render the allowed-command surface as sorted lines; sections with no entries
/// are omitted.
///
/// The lines are `Allowed`, `Allowed in pipelines`, `Denied subcommands`,
/// `Denied flags`, `Restricted to forms`, and `Script hosts`.
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

    if !commands.allow_flags.is_empty() {
        let mut form_pairs: Vec<String> = Vec::new();
        for (cmd, forms) in &commands.allow_flags {
            let mut sorted_forms: Vec<&str> = forms.iter().map(String::as_str).collect();
            sorted_forms.sort_unstable();
            for form in sorted_forms {
                form_pairs.push(format!("{cmd} {form}"));
            }
        }
        form_pairs.sort_unstable();
        parts.push(format!("Restricted to forms: {}", form_pairs.join(", ")));
    }

    if !commands.script_hosts.is_empty() {
        let mut sorted: Vec<&str> = commands.script_hosts.iter().map(String::as_str).collect();
        sorted.sort_unstable();
        parts.push(format!("Script hosts: {}", sorted.join(", ")));
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
    // The misc-206 honesty line, appended to every non-opaque teaching below:
    // a denied compound's earlier write legs never ran, and the denial says so
    // by name (bug 117). `None` — silence — for a single-statement denial or a
    // compound with no resolved earlier writes.
    let append_skipped = |mut msg: String| {
        if let Some(note) = format_skipped_writes_note(&denial.skipped_writes) {
            msg.push('\n');
            msg.push_str(&note);
        }
        msg
    };

    // Opaque-write denial: the resolver's construct-naming teaching message
    // is the whole denial — it already names the resolvable alternative
    // (ws38 ticket 01), independent of guidance entries and build hints.
    if denial.reason == DenialReason::OpaqueWrite {
        if let Some(msg) = &denial.message {
            return msg.clone();
        }
        return format_opening_line(denied_cmd, denial.reason);
    }

    // A denied subcommand carrying a teaching message (the built-in `git
    // worktree` deny, misc 151) surfaces that message as the whole denial — it
    // names the sanctioned `catenary worktree` replacement. Config-sourced
    // denied subcommands (`git grep`) carry no message and fall through to the
    // generic opening line below.
    if denial.reason == DenialReason::DeniedSubcommand
        && let Some(msg) = &denial.message
    {
        return append_skipped(msg.clone());
    }

    // Form-lever denial (`allow_flags`): the config-sourced message naming the
    // permitted forms is the whole denial — it is the lever's teaching surface.
    if denial.reason == DenialReason::DisallowedForm {
        let lookup_cmd = denied_cmd.split_whitespace().next().unwrap_or(denied_cmd);
        if let Some(forms) = commands.allow_flags.get(lookup_cmd) {
            return append_skipped(format_disallowed_form_denial(lookup_cmd, forms));
        }
        return append_skipped(format_opening_line(denied_cmd, denial.reason));
    }

    // Guidance hint (static, build-resolved, or redirect).
    // For the full dump, the base command name is used for lookup (strip
    // subcommand part: "git grep" → "git" won't match, but "grep" will).
    let lookup_cmd = denied_cmd.split_whitespace().next().unwrap_or(denied_cmd);

    // Redirect denial: short format with the command's `-h` output. The
    // skipped-writes note rides directly after the redirect teaching — before
    // the help dump — so the agent reads "never ran" before it follows the
    // redirect to the named path (bug 117's exact trap).
    if let Some(crate::config::GuidanceEntry::Redirect { command }) =
        commands.guidance_for(lookup_cmd)
    {
        let opening = append_skipped(format!(
            "`{denied_cmd}` isn't allowed. Use `catenary {command}` instead. Works on any path (LSP enrichment only within tracked roots)."
        ));
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

    append_skipped(parts.join("\n"))
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
    /// mirror to drift (bugs/12). The template pipeline excludes `awk`, while
    /// `sed`/`perl` are allowed bulk writers whose in-place edits the resolver
    /// script-checks (ws38 ticket 06). (The `python_equivalent`/`basic`
    /// fixtures keep a permissive pipeline on purpose — they test parser
    /// mechanics, not the default.)
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
    fn sed_write_command_denied_mid_pipeline() {
        // `sed` is allowlisted (ws38 06), but its in-script `w`/`e` writes/execs
        // are unattributable — the resolver denies them wherever they appear,
        // including mid-pipeline where the program string would otherwise slip by.
        let rules = recommended_rules();
        assert!(check_command("git log | sed -n 'w /tmp/x'", &rules, None).is_some());
    }

    #[test]
    fn sed_write_command_denied_with_teaching_message() {
        let rules = recommended_rules();
        let denial = check_command("git log | sed -n 'w /tmp/x'", &rules, None)
            .expect("sed `w` write denied by the resolver");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
        let msg = format_denial(&denial.command, &rules, &denial, None, None);
        // The resolver's construct-naming teaching message points at the tracked
        // form (`sed -i` on the target files), not at the retired `catenary sed`.
        assert!(msg.contains("sed -i"), "teaches native sed -i: {msg}");
        assert!(
            !msg.contains("catenary sed"),
            "must not point at the retired catenary sed: {msg}"
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

    #[test]
    fn unbounded_interpreters_stay_denied() {
        // ws38 ticket 04: awk/perl gain a checkable subset, but the unbounded
        // languages admit none — they stay denied by name (not in allow /
        // pipeline / build), even when their program obviously writes.
        let rules = recommended_rules();
        for cmd in [
            "python -c \"open('f','w').write('x')\"",
            "ruby -e 'File.write(\"f\", \"x\")'",
            "node -e 'require(\"fs\").writeFileSync(\"f\", \"x\")'",
            "make test | python -c \"import sys; open('f','w')\"",
        ] {
            assert!(
                check_command(cmd, &rules, None).is_some(),
                "unbounded interpreter denied: {cmd}",
            );
        }
    }

    #[test]
    fn allowlisted_awk_system_still_denied_by_resolver() {
        // The bug-12 awk/sed hazard is not a hardcoded denylist branch — it is
        // config exclusion (awk/sed off the recommended pipeline) *plus* the
        // resolver's program check. Proof the resolver covers it: even with awk
        // allowlisted mid-pipeline, an in-program `system()` denies as an
        // OpaqueWrite (construct-naming), never silently allowed.
        let mut rules = basic_rules();
        rules.pipeline.insert("awk".into());
        let denial = check_command("echo x | awk 'BEGIN{system(\"rm -rf x\")}'", &rules, None)
            .expect("awk system() denied by the resolver despite allowlisting");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
        // A pure filter with the same allowlisting is allowed — the check is on
        // the program, not the name.
        assert!(
            check_command("echo x | awk '{print $1}'", &rules, None).is_none(),
            "a pure awk filter stays allowed",
        );
    }

    #[test]
    fn allowlisted_interpreter_inline_code_is_opaque_write() {
        // ws38 ticket 05: even when a config *allowlists* the interpreter, inline
        // code is denied as an OpaqueWrite at the resolver — the allowlist can no
        // longer create an unattributed write. Plain script execution stays
        // allowed (the executor boundary).
        let mut rules = basic_rules();
        rules.allow.insert("python".into());
        let denial = check_command("python -c \"open('f','w')\"", &rules, None)
            .expect("inline python code denied despite allowlisting");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
        assert!(
            check_command("python script.py", &rules, None).is_none(),
            "a plain script executor stays allowed",
        );
    }

    // ── reads moved to `allow` (Decision 7, drop read-blocking) ───────

    #[test]
    fn cat_allowed_no_redirect() {
        // `cat` reads a file to stdout — no write vector, so it is allowed.
        let rules = recommended_rules();
        assert!(check_command("cat src/main.rs", &rules, None).is_none());
    }

    #[test]
    fn cat_redirect_resolves_or_denies() {
        // ws38 ticket 01: the blanket redirect deny is flipped to
        // resolve-or-deny. A literal target resolves (its write-set is
        // produced for attribution); an opaque one still denies.
        let rules = recommended_rules();
        assert!(
            check_command("cat foo > bar.rs", &rules, None).is_none(),
            "a resolvable redirect is allowed",
        );
        let denial =
            check_command("cat foo > $TARGET", &rules, None).expect("opaque redirect denied");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
    }

    #[test]
    fn unquoted_heredoc_substitution_is_gated() {
        // Bug 46: an *unquoted* `<<EOF` body is not opaque stdin — the shell
        // expands `$(…)` / `` `…` `` in it and runs them. `cat` (which owns the
        // heredoc) is allowed, but an unattributable writer smuggled into the
        // body via a substitution must still be gated, just as the bare command
        // is. Native `sed -i` is a tracked write now (ws38 06), so the smuggled
        // command here is a sed `w` write — opaque, so the resolver denies it.
        let rules = recommended_rules();
        assert!(
            check_command("cat <<EOF\n`sed -i 'w hijack' f`\nEOF", &rules, None).is_some(),
            "backtick substitution in an unquoted heredoc body must be gated",
        );
        assert!(
            check_command("cat <<EOF\n$(sed -i 'w hijack' f)\nEOF", &rules, None).is_some(),
            "$(…) substitution in an unquoted heredoc body must be gated",
        );
    }

    #[test]
    fn quoted_heredoc_substitution_stays_inert() {
        // A *quoted* delimiter (`<<'EOF'`) performs no expansion, so the same
        // `$(…)` text is literal stdin and `cat` alone runs — nothing is denied.
        let rules = recommended_rules();
        assert!(
            check_command("cat <<'EOF'\n$(sed -i 'w hijack' f)\nEOF", &rules, None).is_none(),
            "quoted-delimiter heredoc body is inert",
        );
        // And bare prose in an *unquoted* body (no real substitution) is still
        // not a command — only `$(…)` / `` `…` `` spans project, not words (bug 17).
        assert!(
            check_command(
                "cat <<EOF\njust prose mentioning sed and rm\nEOF",
                &rules,
                None
            )
            .is_none(),
            "prose in an unquoted heredoc body is not a command",
        );
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
        // stdout-only generators are allowed; their resolvable redirect forms
        // are allowed too (ws38: the write-set resolves), while an opaque
        // target still denies.
        let rules = recommended_rules();
        for cmd in ["echo hello", "printf '%s' x", "seq 1 5"] {
            assert!(check_command(cmd, &rules, None).is_none(), "{cmd} allowed");
        }
        for cmd in ["echo hello > f.txt", "printf x > f.txt", "seq 1 5 > f.txt"] {
            assert!(
                check_command(cmd, &rules, None).is_none(),
                "{cmd} resolvable redirect allowed",
            );
        }
        // Opaque targets: an unbound variable and a command-substitution
        // target (its inner command is allowlisted, so the resolver's
        // classification — not the allowlist — is what denies).
        for cmd in ["echo hello > $F", "seq 1 5 > $(cat names.txt)"] {
            let denial = check_command(cmd, &rules, None).expect("opaque redirect denied");
            assert_eq!(denial.reason, DenialReason::OpaqueWrite, "{cmd} denied");
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

    // ── Native `grep` uniformly deniable (pipeable-output ticket 05) ──
    //
    // Once `catenary grep` reads stdin (ticket 04) and runs downstream of a pipe,
    // native `grep` no longer needs the mid-pipe carve-out (the `pipeline` list).
    // A config that lists `grep` in neither `allow` nor `pipeline` therefore
    // denies it *uniformly* — first command *and* mid-pipe, with no positional
    // exception — and (via the scan redirect) nudges to `catenary grep` in every
    // position. The recommended default keeps `grep` in the pipeline; removing it
    // is a per-config choice this mechanism enables, not one catenary forces.

    /// Recommended rules with `grep` removed from the pipeline — the per-config
    /// "deny native grep" choice. The scan→`catenary grep` redirect guidance
    /// (shipped with the recommended config) is retained.
    fn rules_denying_grep() -> ResolvedCommands {
        let mut rules = recommended_rules();
        rules.pipeline.remove("grep");
        rules
    }

    #[test]
    fn native_grep_denied_first_command_and_mid_pipe() {
        let rules = rules_denying_grep();
        // First command: denied (no positional carve-out).
        let first = check_command("grep pattern src", &rules, None)
            .expect("native grep denied as the first command");
        assert_eq!(first.reason, DenialReason::NotAllowed);
        // Mid-pipe: denied too — the carve-out is gone. `cat` is allowlisted, so
        // the denial lands specifically on `grep`, not the upstream.
        let mid = check_command("cat src/main.rs | grep pattern", &rules, None)
            .expect("native grep denied mid-pipe");
        assert_eq!(mid.command, "grep");
        assert_eq!(mid.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn native_grep_denial_redirects_to_catenary_grep_uniformly() {
        let rules = rules_denying_grep();
        for cmd in ["grep pattern src", "cat src/main.rs | grep pattern"] {
            let denial = check_command(cmd, &rules, None).expect("native grep denied");
            let msg = format_denial(&denial.command, &rules, &denial, None, None);
            assert!(
                msg.contains("catenary grep"),
                "`{cmd}` must nudge to catenary grep, got: {msg}",
            );
        }
    }

    // ── Heredoc is stdin input, not an allow/deny knob (ticket 08) ────
    //
    // The heredoc allowlist exception is deleted (decision 020 §2): a command
    // faces the allowlist on its *name* alone, never on the form in which it
    // receives stdin. `<<` lexes to `RedirectOp::Read` (input), so it never
    // shields an output redirect, and the body is stripped before any gate sees
    // it. So denial moves to the command, where it belongs: `cat <<EOF` is
    // allowed iff `cat` is allowed; `python <<EOF` denies because `python` is.

    #[test]
    fn cat_heredoc_allowed_when_cat_allowed() {
        // `cat` is on the recommended allowlist (reads moved to `allow`), so
        // `cat <<EOF` is allowed — because `cat` is allowed, not via a heredoc
        // exception.
        let rules = recommended_rules();
        assert!(check_command("cat <<EOF\nhello\nEOF", &rules, None).is_none());
    }

    #[test]
    fn cat_heredoc_denied_when_cat_not_allowed() {
        // Flip from the old heredoc exception: with `cat` *not* allowlisted, the
        // heredoc form no longer waves it through — the command faces the
        // allowlist normally and is denied.
        let rules = basic_rules();
        let denial =
            check_command("cat <<EOF\nhello\nEOF", &rules, None).expect("cat denied on its name");
        assert_eq!(denial.command, "cat");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn python_heredoc_denied() {
        // Required ticket-08 outcome: `python <<EOF` now DENIES — `python` is
        // denied regardless of the stdin form. (Was allowed under the old
        // heredoc exception.)
        let rules = recommended_rules();
        let denial = check_command("python <<EOF\nprint(1)\nEOF", &rules, None)
            .expect("python denied on its name");
        assert_eq!(denial.command, "python");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn cat_file_denied() {
        let rules = basic_rules();
        assert!(check_command("cat file.txt", &rules, None).is_some());
    }

    #[test]
    fn head_heredoc_denied_when_not_allowed() {
        // Flip: `head` was waved through by the old quoted-marker heredoc
        // exception. It now faces the allowlist on its name and is denied.
        let mut rules = ResolvedCommands::default();
        rules.allow.insert("git".to_string());
        let denial = check_command("head <<'MARKER'\nhello\nMARKER", &rules, None)
            .expect("head denied on its name");
        assert_eq!(denial.command, "head");
    }

    #[test]
    fn sed_heredoc_denied_mid_position_zero() {
        // Flip: `sed 's/foo/bar/' <<EOF` was allowed by the heredoc exception.
        // `sed` is a pipeline command, so at position 0 it now denies on the
        // pipeline-position guard — the heredoc form is irrelevant.
        let rules = basic_rules();
        let denial = check_command("sed 's/foo/bar/' <<EOF\nhello\nEOF", &rules, None)
            .expect("sed denied at pipeline position 0");
        assert_eq!(denial.command, "sed");
        assert_eq!(denial.reason, DenialReason::PipelinePosition);
    }

    #[test]
    fn heredoc_unquoted_arg_before_heredoc_still_denied() {
        // `grep pattern <<EOF` — grep is a pipeline command at position 0, so it
        // denies. (Outcome preserved; the old "leading-heredoc narrowing"
        // reasoning is gone — the form never mattered.)
        let rules = basic_rules();
        assert!(check_command("grep pattern <<EOF\nhello\nEOF", &rules, None).is_some());
    }

    #[test]
    fn heredoc_file_arg_before_heredoc_still_denied() {
        // `cat file.txt <<EOF` — cat is not allowlisted, so it denies regardless
        // of the heredoc. (Outcome preserved.)
        let rules = basic_rules();
        assert!(check_command("cat file.txt <<EOF\nhello\nEOF", &rules, None).is_some());
    }

    // ── git worktree — the built-in agent-surface deny (misc 151) ────

    #[test]
    fn git_worktree_denied_with_catenary_pointer() {
        // `git` is allowlisted in basic_rules, yet every `git worktree`
        // subcommand is denied on the agent surface, with a teaching message
        // pointing at `catenary worktree`.
        let rules = basic_rules();
        for cmd in [
            "git worktree add ../wt topic",
            "git worktree list",
            "git worktree remove ../wt",
        ] {
            let denial = check_command(cmd, &rules, None).expect("git worktree must be denied");
            assert_eq!(denial.command, "git worktree");
            assert_eq!(denial.reason, DenialReason::DeniedSubcommand);
            let msg = format_denial(&denial.command, &rules, &denial, None, None);
            assert!(
                msg.contains("catenary worktree"),
                "`{cmd}` denial must point at catenary worktree, got: {msg}",
            );
            // misc 177: the pointer also teaches the dispatch flow, so a
            // WorktreeCreate client is never bounced from this message into
            // the `catenary worktree add` dispatch deny.
            assert!(
                msg.contains("isolation: \"worktree\""),
                "`{cmd}` denial must teach isolation dispatch, got: {msg}",
            );
        }
    }

    #[test]
    fn git_non_worktree_subcommands_still_allowed() {
        // The built-in deny is surgical: other `git` subcommands are unaffected.
        let rules = basic_rules();
        assert!(check_command("git status", &rules, None).is_none());
        assert!(check_command("git commit -m x", &rules, None).is_none());
    }

    // ── Subagent branch guard (misc 221) ─────────────────────────────
    //
    // A worktree-anchored subagent must not manipulate branches in a repo
    // OUTSIDE its anchored worktree — the incident was a worker leaving the
    // SHARED checkout on a stray branch. Scope is subagents ONLY (the lead is
    // explicitly untouched, maintainer ruling 2026-07-23); the guard fires only
    // when a `-C`/`--git-dir`/`--work-tree` target names a repo outside the
    // anchor.

    /// A subagent context anchored at `/wt/agent` — the standard fixture for the
    /// guard tests below.
    fn subagent_at(anchor: &str) -> SessionContext {
        SessionContext::Subagent {
            anchor: Some(std::path::PathBuf::from(anchor)),
        }
    }

    #[test]
    fn subagent_branch_ops_denied_outside_anchor() {
        // Every branch-manipulating form, targeting the SHARED checkout outside
        // the subagent's worktree, is denied.
        let rules = basic_rules();
        let sub = subagent_at("/wt/agent");
        for cmd in [
            "git -C /shared/repo switch main",
            "git -C /shared/repo checkout -b topic",
            "git -C /shared/repo checkout -B topic",
            "git -C /shared/repo checkout main",
            "git -C /shared/repo branch newbranch",
            "git -C /shared/repo branch -d oldbranch",
            "git -C /shared/repo branch -D oldbranch",
            "git -C /shared/repo branch -m old new",
            "git --git-dir /shared/repo/.git branch feature",
            "git --work-tree /shared/repo switch main",
            "git --git-dir=/shared/repo/.git switch main",
            // A relative target escaping the anchor is still outside.
            "git -C ../shared switch main",
        ] {
            let denial = check_command_in_session(cmd, &rules, None, &sub)
                .expect("branch op outside the anchor must be denied for a subagent");
            assert_eq!(denial.reason, DenialReason::DeniedSubcommand);
            let msg = format_denial(&denial.command, &rules, &denial, None, None);
            assert!(
                msg.contains("/wt/agent"),
                "`{cmd}` denial must name the anchored worktree, got: {msg}",
            );
            assert!(
                msg.contains("anchored worktree"),
                "`{cmd}` denial must teach that branch work belongs in the anchor, got: {msg}",
            );
        }
    }

    #[test]
    fn subagent_branch_ops_allowed_inside_anchor() {
        // A branch op with NO external target operates on the anchored worktree's
        // own repo — the sanctioned place for the worker's branch work.
        let rules = basic_rules();
        let sub = subagent_at("/wt/agent");
        for cmd in [
            "git switch main",
            "git checkout -b topic",
            "git checkout main",
            "git branch newbranch",
            "git branch -d oldbranch",
            // An explicit target INSIDE the anchor (a subdir) is still inside.
            "git -C /wt/agent switch main",
            "git -C /wt/agent/sub branch feature",
            // A relative target that stays within the anchor.
            "git -C ./sub switch main",
        ] {
            assert!(
                check_command_in_session(cmd, &rules, None, &sub).is_none(),
                "`{cmd}` must be allowed for a subagent inside its anchor",
            );
        }
    }

    #[test]
    fn subagent_non_branch_git_ops_untouched_even_outside_anchor() {
        // The guard is surgical: only BRANCH manipulation is guarded. A subagent
        // may still run non-branch git against a repo it names (`git -C` status /
        // log / a pathspec restore / a plain commit) — those are not the
        // stray-branch hazard.
        let rules = basic_rules();
        let sub = subagent_at("/wt/agent");
        for cmd in [
            "git -C /shared/repo status",
            "git -C /shared/repo log --oneline",
            "git -C /shared/repo commit -m x",
            // A pathspec restore is not a branch move.
            "git -C /shared/repo checkout -- src/main.rs",
            "git -C /shared/repo checkout main -- src/main.rs",
            // A read-only branch listing manipulates nothing.
            "git -C /shared/repo branch --list",
            "git -C /shared/repo branch -a",
        ] {
            // The branch GUARD must not fire for these — proven by the subagent
            // verdict matching the lead verdict (the guard is the only difference
            // between the two contexts). The write resolver may independently
            // deny a pathspec checkout as an opaque write, but that verdict is
            // identical for lead and subagent, so this isolates the guard.
            let lead = check_command_in_session(cmd, &rules, None, &SessionContext::Lead)
                .map(|d| d.command);
            let subagent = check_command_in_session(cmd, &rules, None, &sub).map(|d| d.command);
            assert_eq!(
                subagent, lead,
                "`{cmd}` is not branch manipulation — the guard must not change \
                 the verdict vs a lead session",
            );
        }
    }

    #[test]
    fn lead_branch_ops_untouched_outside_any_repo() {
        // The binding scope: the lead / top-level agent is EXPLICITLY untouched.
        // The same cross-repo branch ops that a subagent is denied run freely for
        // a lead — both via the lead-context entry point and the default
        // `check_command`.
        let rules = basic_rules();
        for cmd in [
            "git -C /shared/repo switch main",
            "git -C /shared/repo checkout -b topic",
            "git -C /shared/repo branch -D oldbranch",
        ] {
            assert!(
                check_command_in_session(cmd, &rules, None, &SessionContext::Lead).is_none(),
                "`{cmd}` must be untouched for a lead session",
            );
            assert!(
                check_command(cmd, &rules, None).is_none(),
                "`{cmd}` must be untouched on the default (lead) entry point",
            );
        }
    }

    #[test]
    fn subagent_branch_guard_inert_with_no_anchor() {
        // A subagent whose hook carried no cwd has no anchor to compare against,
        // so no target can be established as "outside" — the guard fails open
        // rather than denying blind.
        let rules = basic_rules();
        let sub = SessionContext::Subagent { anchor: None };
        assert!(
            check_command_in_session("git -C /shared/repo switch main", &rules, None, &sub)
                .is_none(),
            "with no anchor the guard cannot establish an outside target — fail open",
        );
    }

    #[test]
    fn subagent_branch_guard_survives_global_option_shuffle() {
        // The bug-140 vocabulary: a value-carrying non-target global (`-c
        // key=val`) before the target/subcommand must not shift the read. The
        // guard still sees the `-C` target and the `switch` subcommand.
        let rules = basic_rules();
        let sub = subagent_at("/wt/agent");
        assert!(
            check_command_in_session(
                "git -c core.pager=cat -C /shared/repo switch main",
                &rules,
                None,
                &sub,
            )
            .is_some(),
            "a non-target global before `-C` must not hide the cross-repo switch",
        );
    }

    #[test]
    fn subagent_branch_guard_unresolvable_target_fails_open() {
        // An unresolvable target (a variable) can't be established as outside the
        // anchor — fail open, matching the `cd`-target conservatism.
        let rules = basic_rules();
        let sub = subagent_at("/wt/agent");
        assert!(
            check_command_in_session("git -C $REPO switch main", &rules, None, &sub).is_none(),
            "a variable `-C` target is unresolvable — the guard fails open",
        );
    }

    #[test]
    fn subagent_branch_guard_distinguishes_global_c_from_branch_copy_c() {
        // `git -C <path>` is the repo-targeting GLOBAL; `git branch -C` is the
        // branch-COPY flag (after the subcommand). The phase-1 walk stops at
        // `branch`, so a post-subcommand `-C` is never mistaken for a target.
        let rules = basic_rules();
        let sub = subagent_at("/wt/agent");
        // Global `-C` outside + `branch -C` copy → denied (cross-repo copy).
        assert!(
            check_command_in_session("git -C /shared/repo branch -C old new", &rules, None, &sub)
                .is_some(),
            "`git -C /shared branch -C` copies a branch in the shared repo — denied",
        );
        // No global target, only the branch-copy `-C` in the anchor's own repo →
        // allowed (branch work in the anchored worktree is sanctioned).
        assert!(
            check_command_in_session("git branch -C old new", &rules, None, &sub).is_none(),
            "`git branch -C` with no external target is anchor-local branch work",
        );
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

    // ── Subcommand deny survives global-option shuffle (bug 140) ─────
    //
    // A denied subcommand is the first *positional* past the command's global
    // options — so a value-carrying global (`-C <path>`, `-c key=val`,
    // `--git-dir …`) can never shift the real subcommand out of the matched
    // position. Each of these forms was allowed pre-fix (the naive check read
    // only `argv.first()`).

    #[test]
    fn git_grep_denied_behind_capital_c_option() {
        // The lead's first-hand repro: `git -C <path> grep …` slipped through.
        let rules = basic_rules();
        assert!(
            check_command("git -C /some/path grep -c pattern -- src", &rules, None).is_some(),
            "`git -C <path> grep` must still resolve to the denied `git grep`",
        );
    }

    #[test]
    fn git_grep_denied_behind_config_option() {
        let rules = basic_rules();
        assert!(
            check_command("git -c core.pager=cat grep pattern", &rules, None).is_some(),
            "`git -c key=val grep` must resolve to the denied `git grep`",
        );
    }

    #[test]
    fn git_grep_denied_behind_git_dir_glued() {
        let rules = basic_rules();
        assert!(
            check_command("git --git-dir=/repo/.git grep pattern", &rules, None).is_some(),
            "`git --git-dir=… grep` must resolve to the denied `git grep`",
        );
    }

    #[test]
    fn git_grep_denied_behind_git_dir_separated() {
        let rules = basic_rules();
        assert!(
            check_command("git --git-dir /repo/.git grep pattern", &rules, None).is_some(),
            "`git --git-dir … grep` (separated value) must resolve to `git grep`",
        );
    }

    #[test]
    fn git_grep_denied_behind_work_tree_separated() {
        let rules = basic_rules();
        assert!(
            check_command("git --work-tree /repo grep pattern", &rules, None).is_some(),
            "`git --work-tree … grep` must resolve to `git grep`",
        );
    }

    #[test]
    fn git_grep_denied_behind_paginate_boolean() {
        // `-p` / `--paginate` are boolean globals — they take no value, so the
        // very next token is the subcommand.
        let rules = basic_rules();
        assert!(
            check_command("git -p grep pattern", &rules, None).is_some(),
            "`git -p grep` (boolean global) must resolve to `git grep`",
        );
        assert!(
            check_command("git --paginate grep pattern", &rules, None).is_some(),
            "`git --paginate grep` must resolve to `git grep`",
        );
    }

    #[test]
    fn git_grep_denied_behind_stacked_globals() {
        // `git -C x -c a=b grep …` — the ruling's stacked-combination case.
        let rules = basic_rules();
        assert!(
            check_command("git -C x -c a=b grep pattern", &rules, None).is_some(),
            "stacked globals before `grep` must still resolve to `git grep`",
        );
        assert!(
            check_command(
                "git --git-dir=/r/.git -c a=b -C x grep pattern",
                &rules,
                None
            )
            .is_some(),
            "deeply stacked globals before `grep` must resolve to `git grep`",
        );
    }

    #[test]
    fn git_ls_files_denied_behind_globals() {
        // The whole denied surface, not just `grep`.
        let rules = basic_rules();
        assert!(
            check_command("git -C /some/path ls-files", &rules, None).is_some(),
            "`git -C <path> ls-files` must resolve to `git ls-files`",
        );
        assert!(
            check_command("git -c a=b ls-tree HEAD", &rules, None).is_some(),
            "`git -c key=val ls-tree` must resolve to `git ls-tree`",
        );
    }

    #[test]
    fn git_commit_still_allowed_behind_globals() {
        // The fix must not over-deny: a *non-denied* subcommand behind the same
        // globals is still allowed.
        let rules = basic_rules();
        assert!(
            check_command("git -C /some/path status", &rules, None).is_none(),
            "`git -C <path> status` is not denied",
        );
        assert!(
            check_command("git -c core.editor=vim commit -m x", &rules, None).is_none(),
            "`git -c key=val commit` is not denied",
        );
    }

    #[test]
    fn git_ambiguous_leading_long_option_fails_closed() {
        // A leading long option we can't prove is boolean *could* be consuming
        // the token that would otherwise be the subcommand: if the ambiguity
        // could hide a denied subcommand, deny (fail closed). Here the token
        // after an unknown-arity option is `grep`, a denied subcommand — under
        // the boolean reading it IS the subcommand, so we must deny.
        let rules = basic_rules();
        assert!(
            check_command("git --unknown-opt grep pattern", &rules, None).is_some(),
            "an unknown-arity leading option before `grep` must fail closed",
        );
    }

    #[test]
    fn git_ambiguous_short_cluster_fails_closed() {
        // A short cluster of unknown arity before a denied subcommand: the
        // boolean reading places `grep` in subcommand position, so fail closed.
        let rules = basic_rules();
        assert!(
            check_command("git -q grep pattern", &rules, None).is_some(),
            "an unknown-arity short flag before `grep` must fail closed",
        );
    }

    // ── sqlite3 `-cmd` is a flag-shaped denied subcommand (bug 140) ──
    //
    // The denied token itself is a flag (`sqlite3 -cmd`), not a positional. It
    // must be caught wherever it sits in the leading option run, not only at
    // `argv[0]`.

    fn sqlite_rules() -> ResolvedCommands {
        ResolvedCommands {
            allow: HashSet::from(["sqlite3".into()]),
            deny: HashMap::from([("sqlite3".into(), HashSet::from(["-cmd".into()]))]),
            ..ResolvedCommands::default()
        }
    }

    #[test]
    fn sqlite_cmd_denied_at_front() {
        let rules = sqlite_rules();
        assert!(
            check_command("sqlite3 -cmd \".mode csv\" db.sqlite", &rules, None).is_some(),
            "`sqlite3 -cmd …` must be denied",
        );
    }

    #[test]
    fn sqlite_cmd_denied_after_other_flags() {
        // `-cmd` shuffled behind other options must still be caught.
        let rules = sqlite_rules();
        assert!(
            check_command(
                "sqlite3 -readonly -cmd \".mode csv\" db.sqlite",
                &rules,
                None
            )
            .is_some(),
            "`sqlite3 -readonly -cmd …` must be denied",
        );
        assert!(
            check_command("sqlite3 -batch -header -cmd \".dump\" db", &rules, None).is_some(),
            "`-cmd` behind several boolean flags must be denied",
        );
    }

    #[test]
    fn sqlite_without_cmd_allowed() {
        let rules = sqlite_rules();
        assert!(
            check_command("sqlite3 -readonly db.sqlite \"select 1\"", &rules, None).is_none(),
            "a `sqlite3` invocation with no `-cmd` is allowed",
        );
    }

    #[test]
    fn cargo_not_allowed() {
        let rules = basic_rules();
        assert!(check_command("cargo test", &rules, None).is_some());
        assert!(check_command("cargo clippy", &rules, None).is_some());
    }

    #[test]
    fn recommended_config_denies_shuffled_subcommands() {
        // End-to-end against the *shipped* recommendation (bug 140): the
        // global-option shuffle is closed on the real surface, not just the
        // hand-built fixtures. `git`'s value-carrying globals and `sqlite3`'s
        // flag-shuffle both resolve to the denied token.
        let rules = recommended_rules();
        assert!(
            check_command("git -C /repo grep pattern", &rules, None).is_some(),
            "`git -C <path> grep` denied on the shipped surface",
        );
        assert!(
            check_command("git -c a=b ls-files", &rules, None).is_some(),
            "`git -c key=val ls-files` denied on the shipped surface",
        );
        assert!(
            check_command("sqlite3 -readonly -cmd \".dump\" db", &rules, None).is_some(),
            "`sqlite3 -readonly -cmd …` denied on the shipped surface",
        );
        // And a plain read past the same globals is still allowed.
        assert!(
            check_command("git -C /repo log --oneline", &rules, None).is_none(),
            "`git -C <path> log` remains allowed",
        );
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
            // Non-recursive cp resolves even without a cwd (both landing
            // interpretations recorded — over-recording, safe).
            assert!(check_command("cp foo bar", &rules, None).is_none());
            // mv must query the source's dir-ness (a directory source moves a
            // whole tree), so it needs the hook cwd — which production always
            // has.
            let tmp = tempfile::tempdir().expect("tempdir");
            std::fs::write(tmp.path().join("foo"), b"x").expect("touch foo");
            assert!(check_command("mv foo bar", &rules, Some(tmp.path())).is_none());
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
        //
        // The multi-line `git commit`/`gh pr create` heredoc form runs a *real*
        // `cat` inside `$(…)` (the shell captures its output), so it is allowed
        // because `cat` is allowlisted — `recommended_rules` (matching the
        // shipped config) puts reads in `allow`. The old heredoc *exception*
        // that waved the inner `cat` through regardless is deleted (ticket 08);
        // the body prose is still stripped before any gate sees it, so the
        // semicolons / parentheses in the message never segment.
        #[test]
        fn git_commit_heredoc() {
            let rules = recommended_rules();
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
            let rules = recommended_rules();
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
            let rules = recommended_rules();
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
            let rules = recommended_rules();
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

    // The mask_quotes / strip_heredoc_bodies / find_command / pipe_split unit
    // tests retired with their scanners (tokenizer ticket 08): segmentation,
    // env-var skipping, heredoc stripping, and quote-faithful pipe splitting are
    // now properties of the one faithful parse, covered by `parse.rs` tests.

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
            message: None,
            skipped_writes: Vec::new(),
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
        assert!(
            !surface.contains("Restricted to forms"),
            "empty allow_flags should be omitted"
        );
        assert!(
            !surface.contains("Script hosts"),
            "empty script_hosts should be omitted"
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
        let home = crate::paths::home_dir().expect("HOME");
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
            message: None,
            skipped_writes: Vec::new(),
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
            message: None,
            skipped_writes: Vec::new(),
        };
        let msg = format_denial("npm", &rules, &denial, None, None);
        assert!(
            !msg.contains("could not be resolved"),
            "should not include note when resolved: {msg}",
        );
    }

    // ── Skipped-writes note (misc 206, bug 117) ───────────────────────

    /// The live-config shape of bug 117: `grep` carries a Redirect guidance
    /// entry, so the denial teaching is "Use `catenary grep` instead".
    fn rules_with_grep_redirect() -> ResolvedCommands {
        let mut rules = basic_rules();
        rules.guidance.insert(
            "grep".to_string(),
            crate::config::GuidanceEntry::Redirect {
                command: "grep".to_string(),
            },
        );
        rules
    }

    #[test]
    fn compound_denial_names_skipped_earlier_write() {
        // Bug 117's exact shape: the whole compound is denied on the
        // statement-initial `grep`, so `make check` never ran and `check.log`
        // was never written — the teaching must say so by name, or the agent
        // follows the redirect and reads a stale log as fresh output.
        let rules = rules_with_grep_redirect();
        let denial = check_command(
            "make check > check.log 2>&1; grep -n err check.log",
            &rules,
            None,
        )
        .expect("statement-initial grep denied");
        assert_eq!(denial.command, "grep");
        assert_eq!(
            denial.skipped_writes,
            vec![std::path::PathBuf::from("check.log")],
        );
        let msg = format_denial(&denial.command, &rules, &denial, None, None);
        assert!(
            msg.contains("Use `catenary grep` instead"),
            "redirect teaching kept: {msg}",
        );
        assert!(
            msg.contains("Note: nothing in this command ran, including the write to `check.log`."),
            "skipped write named with the teaching: {msg}",
        );
    }

    #[test]
    fn single_statement_denial_appends_no_skipped_writes_note() {
        let rules = rules_with_grep_redirect();
        let denial =
            check_command("grep -n err check.log", &rules, None).expect("grep denied at start");
        assert!(denial.skipped_writes.is_empty());
        let msg = format_denial(&denial.command, &rules, &denial, None, None);
        assert!(
            !msg.contains("nothing in this command ran"),
            "no note for a single-statement denial: {msg}",
        );
    }

    #[test]
    fn compound_denied_in_first_statement_appends_no_note() {
        // The denied statement is first — nothing earlier was skipped, so
        // silence is correct even though a later statement carries a write.
        let rules = rules_with_grep_redirect();
        let denial = check_command(
            "grep -n err check.log; make check > check.log 2>&1",
            &rules,
            None,
        )
        .expect("grep denied in the first statement");
        assert!(denial.skipped_writes.is_empty());
        let msg = format_denial(&denial.command, &rules, &denial, None, None);
        assert!(
            !msg.contains("nothing in this command ran"),
            "no note when the denied statement is first: {msg}",
        );
    }

    #[test]
    fn skipped_writes_note_caps_the_list() {
        let rules = rules_with_grep_redirect();
        let denial = check_command(
            "echo a > f1.log; echo b > f2.log; echo c > f3.log; echo d > f4.log; grep err f1.log",
            &rules,
            None,
        )
        .expect("grep denied after four writes");
        let msg = format_denial(&denial.command, &rules, &denial, None, None);
        assert!(
            msg.contains("including the writes to `f1.log`, `f2.log`, `f3.log`, and 1 more."),
            "three named targets plus a count: {msg}",
        );
    }

    #[test]
    fn unresolved_earlier_write_names_no_paths() {
        // An earlier statement whose write target doesn't resolve (`$VAR`)
        // must not invent paths — the note stays silent rather than guessing.
        let rules = rules_with_grep_redirect();
        let denial =
            check_command("echo hi > $TARGET; grep err f.log", &rules, None).expect("grep denied");
        assert!(denial.skipped_writes.is_empty(), "no invented paths");
        let msg = format_denial(&denial.command, &rules, &denial, None, None);
        assert!(
            !msg.contains("nothing in this command ran"),
            "silence when earlier writes don't resolve: {msg}",
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
            message: None,
            skipped_writes: Vec::new(),
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
            message: None,
            skipped_writes: Vec::new(),
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
            message: None,
            skipped_writes: Vec::new(),
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
    fn format_full_read_edit_template_vars_antigravity() {
        let rules = rules_with_guidance();
        let denial = no_cd_denial("cat");
        let msg = format_denial(
            "cat",
            &rules,
            &denial,
            Some(crate::cli::HostFormat::Antigravity),
            None,
        );
        assert!(
            msg.contains("Hint: Use read_file instead"),
            "{{READ}} should resolve to read_file for Antigravity: {msg}",
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
            message: None,
            skipped_writes: Vec::new(),
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

    // ── allow_flags: the form lever (misc 127) ──────────────────────

    /// perl allowlisted, restricted to the `-i` / `-pe` invocation forms.
    fn rules_with_allow_flags() -> ResolvedCommands {
        let mut rules = basic_rules();
        rules.allow.insert("perl".into());
        rules.allow_flags =
            HashMap::from([("perl".into(), HashSet::from(["-i".into(), "-pe".into()]))]);
        rules
    }

    #[test]
    fn allow_flags_matching_form_allowed() {
        // `-pe` carries {p, e} ⊇ the `-pe` anchor {p, e}: allowed by the lever,
        // and the resolver sees a pure substitution (NoWrite).
        let rules = rules_with_allow_flags();
        assert!(
            check_command("perl -pe 's/a/b/' f", &rules, None).is_none(),
            "perl -pe should be allowed",
        );
    }

    #[test]
    fn allow_flags_in_place_form_recorded() {
        // `-i -pe` carries {i, p, e}: matches both anchors; the resolver records
        // the in-place write to `f`, so the command is allowed.
        let rules = rules_with_allow_flags();
        assert!(
            check_command("perl -i -pe 's/a/b/' f", &rules, None).is_none(),
            "perl -i -pe should be allowed (write recorded)",
        );
    }

    #[test]
    fn allow_flags_nonmatching_form_denied() {
        // `-ne` carries {n, e}: neither {i} nor {p, e} is a subset, so the lever
        // denies before the resolver — reason DisallowedForm, naming the forms.
        let rules = rules_with_allow_flags();
        let denial =
            check_command("perl -ne 'print' f", &rules, None).expect("perl -ne should be denied");
        assert_eq!(denial.reason, DenialReason::DisallowedForm);
        assert_eq!(denial.command, "perl");
    }

    #[test]
    fn allow_flags_denial_names_permitted_forms() {
        let rules = rules_with_allow_flags();
        let denial = check_command("perl -ne 'print' f", &rules, None).expect("denied");
        let msg = format_denial("perl", &rules, &denial, None, None);
        assert_eq!(
            msg,
            "`perl` is limited by the Catenary configuration to these invocation forms: \
             `-i`, `-pe`. This invocation matches none of them — re-run `perl` in one of \
             the permitted forms.",
        );
    }

    #[test]
    fn allow_flags_extra_flags_do_not_disqualify() {
        // A superset invocation still matches: `-w -pe` carries {w, p, e} ⊇
        // {p, e}. Extra flags are the resolver's business, not the lever's.
        let rules = rules_with_allow_flags();
        assert!(
            check_command("perl -w -pe 's/a/b/' f", &rules, None).is_none(),
            "extra -w must not disqualify the -pe match",
        );
    }

    #[test]
    fn allow_flags_no_entry_is_inert() {
        // sed has no allow_flags entry — the lever never fires for it. (`echo`
        // is the position-0 command here; `sed` is allowed mid-pipeline.)
        let rules = rules_with_allow_flags();
        assert!(
            check_command("echo x | sed 's/a/b/'", &rules, None).is_none(),
            "sed without an allow_flags entry is unaffected",
        );
    }

    #[test]
    fn allow_flags_deny_flags_wins() {
        // deny_flags precedes allow_flags: even a `-pe` form that the lever
        // would permit is denied when `-pe`… carries a denied flag. Here `-w`
        // is denied, and `perl -w -pe` (a matching form) is still denied by the
        // flag denylist.
        let mut rules = rules_with_allow_flags();
        rules.deny_flags = HashMap::from([("perl".into(), HashSet::from(["-w".into()]))]);
        let denial =
            check_command("perl -w -pe 's/a/b/' f", &rules, None).expect("deny_flags wins");
        assert_eq!(denial.reason, DenialReason::DeniedFlag);
        assert_eq!(denial.command, "perl -w");
    }

    #[test]
    fn allow_flags_cannot_reopen_resolver_soundness() {
        // The layering guarantee: a form the lever *permits* is still subject to
        // the resolver. `perl -i script.pl` matches the `-i` anchor, so the
        // lever passes — but the resolver denies the unauditable script file
        // (misc 126). Soundness runs regardless; the lever only narrows.
        let mut rules = basic_rules();
        rules.allow.insert("perl".into());
        rules.allow_flags = HashMap::from([("perl".into(), HashSet::from(["-i".into()]))]);
        let denial =
            check_command("perl -i script.pl", &rules, None).expect("resolver still denies");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
    }

    #[test]
    fn allow_flags_script_file_denied_without_entry() {
        // Baseline for the layering: without any allow_flags entry, the misc-126
        // resolver denial owns `perl script.pl` (soundness, not the lever).
        let mut rules = basic_rules();
        rules.allow.insert("perl".into());
        let denial = check_command("perl script.pl", &rules, None).expect("misc-126 denies");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
    }

    #[test]
    fn allow_flags_e_anchor_matches_carrying_cluster() {
        // Edge case pinned: an `-e` anchor {e} matches any cluster carrying `e`,
        // so with `-e` permitted, `perl -ne` passes the lever (it carries e).
        // The resolver then governs whether the program is checkable.
        let mut rules = basic_rules();
        rules.allow.insert("perl".into());
        rules.allow_flags =
            HashMap::from([("perl".into(), HashSet::from(["-i".into(), "-e".into()]))]);
        // `-ne` carries {n, e} ⊇ {e}: lever passes. `print` is not a checkable
        // substitution, so the resolver denies — not the lever.
        let denial =
            check_command("perl -ne 'print' f", &rules, None).expect("resolver denies program");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
    }

    #[test]
    fn allow_flags_long_form_matched_as_typed() {
        // Long forms are single atoms, distinct from short clusters. A `--foo`
        // anchor matches only when the long flag is present, `=value` stripped.
        let mut rules = basic_rules();
        rules.allow.insert("git".into());
        rules.allow_flags = HashMap::from([("git".into(), HashSet::from(["--no-pager".into()]))]);
        assert!(
            check_command("git --no-pager log", &rules, None).is_none(),
            "matching long form allowed",
        );
        let denial = check_command("git log", &rules, None).expect("no long flag → denied");
        assert_eq!(denial.reason, DenialReason::DisallowedForm);
    }

    #[test]
    fn surface_restricted_forms_section() {
        let rules = rules_with_allow_flags();
        let surface = format_command_surface(&rules).join("\n");
        assert!(
            surface.contains("Restricted to forms:"),
            "should have the form-restriction section: {surface}",
        );
        assert!(
            surface.contains("perl -i"),
            "should list perl -i: {surface}",
        );
        assert!(
            surface.contains("perl -pe"),
            "should list perl -pe: {surface}",
        );
    }

    // ── script_hosts: the executor-boundary opt-in (misc 129) ───────

    /// perl/awk/sed allowlisted, all three opted in as script hosts.
    fn rules_with_script_hosts() -> ResolvedCommands {
        let mut rules = basic_rules();
        rules.allow.insert("perl".into());
        rules.allow.insert("awk".into());
        rules.allow.insert("sed".into());
        rules.script_hosts = HashSet::from(["perl".into(), "awk".into(), "sed".into()]);
        rules
    }

    #[test]
    fn script_hosts_perl_script_file_allowed() {
        // With perl a script host, `perl script.pl [args]` classifies NoWrite
        // (the executor boundary) instead of the misc-126 denial.
        let rules = rules_with_script_hosts();
        assert!(
            check_command("perl script.pl", &rules, None).is_none(),
            "perl script.pl should run as a script host",
        );
        assert!(
            check_command("perl script.pl a b c", &rules, None).is_none(),
            "perl script.pl with args should run as a script host",
        );
    }

    #[test]
    fn script_hosts_perl_stdin_program_allowed() {
        // The bare stdin-program shape (bare `perl`) mirrors the
        // unbounded-interpreter treatment when perl is a script host.
        let rules = rules_with_script_hosts();
        assert!(
            check_command("perl", &rules, None).is_none(),
            "bare perl (stdin program) should run as a script host",
        );
    }

    #[test]
    fn script_hosts_perl_inline_nonsubstitution_still_denied() {
        // Inline `-e` code stays the denied vector even for a script host: a
        // non-substitution program is not checkable (python-consistent).
        let rules = rules_with_script_hosts();
        let denial = check_command("perl -e 'print 1'", &rules, None)
            .expect("perl -e non-substitution still denied");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
    }

    #[test]
    fn script_hosts_perl_inplace_still_resolves_writes() {
        // `-i` keeps its write-set resolution into the diagnostics batch — the
        // script-host opt-in does not blanket-NoWrite an inline substitution.
        let rules = rules_with_script_hosts();
        let writes = check_and_resolve_command("perl -i -pe 's/a/b/' f", &rules, None)
            .expect("perl -i resolves")
            .writes;
        assert!(
            writes.iter().any(|p| p.ends_with("f")),
            "perl -i should still record its in-place write to f: {writes:?}",
        );
    }

    #[test]
    fn script_hosts_awk_program_file_allowed() {
        // awk's `-f progfile` denial relaxes to the executor boundary.
        let rules = rules_with_script_hosts();
        assert!(
            check_command("awk -f prog.awk data.txt", &rules, None).is_none(),
            "awk -f progfile should run as a script host",
        );
    }

    #[test]
    fn script_hosts_sed_script_file_allowed() {
        // sed's `-f scriptfile` denial relaxes to the executor boundary.
        let rules = rules_with_script_hosts();
        assert!(
            check_command("sed -f script.sed data.txt", &rules, None).is_none(),
            "sed -f scriptfile should run as a script host",
        );
    }

    #[test]
    fn script_hosts_absent_keeps_soundness_denial() {
        // Default (no opt-in): every modeled engine's program-file denial stands
        // exactly as misc-126 landed it.
        let mut rules = basic_rules();
        rules.allow.insert("perl".into());
        rules.allow.insert("awk".into());
        rules.allow.insert("sed".into());
        for cmd in [
            "perl script.pl",
            "awk -f prog.awk data.txt",
            "sed -f script.sed data.txt",
        ] {
            let denial = check_command(cmd, &rules, None);
            assert!(denial.is_some(), "{cmd} should still be denied by default");
            assert_eq!(
                denial.expect("denied").reason,
                DenialReason::OpaqueWrite,
                "{cmd}",
            );
        }
    }

    #[test]
    fn script_hosts_only_listed_command_relaxes() {
        // Opting perl in does not relax awk or sed.
        let mut rules = basic_rules();
        rules.allow.insert("perl".into());
        rules.allow.insert("awk".into());
        rules.allow.insert("sed".into());
        rules.script_hosts = HashSet::from(["perl".into()]);
        assert!(
            check_command("perl script.pl", &rules, None).is_none(),
            "perl relaxes",
        );
        assert!(
            check_command("awk -f prog.awk d", &rules, None).is_some(),
            "awk stays denied without its own opt-in",
        );
        assert!(
            check_command("sed -f s.sed d", &rules, None).is_some(),
            "sed stays denied without its own opt-in",
        );
    }

    #[test]
    fn surface_script_hosts_section() {
        let rules = rules_with_script_hosts();
        let surface = format_command_surface(&rules).join("\n");
        let line = surface
            .lines()
            .find(|l| l.starts_with("Script hosts:"))
            .expect("script hosts line");
        assert!(line.contains("awk"), "should list awk: {line}");
        assert!(line.contains("perl"), "should list perl: {line}");
        assert!(line.contains("sed"), "should list sed: {line}");
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

    // ── Write resolution: resolve-or-deny (ws38 ticket 01) ──────────
    //
    // The bug-11 blanket redirect deny is flipped: a redirect whose complete
    // write-set the resolver can see is allowed (the set feeds attribution,
    // ticket 02); an opaque one denies with a construct-naming teaching
    // message. `resolver::tests` owns the per-form coverage; these tests pin
    // the wiring through `check_command`.

    #[test]
    fn resolvable_redirects_are_allowed() {
        // recommended_rules: every command here is allowlisted, so the
        // verdict under test is the resolver's.
        let rules = recommended_rules();
        for cmd in [
            "git status > out.txt",
            "git log >> out.txt",
            "echo x>file",
            "make test 2>file",
            "make test &>file",
            "git status >| out.txt",
            // A heredoc never shields the redirect (bug 11); the `> file.rs`
            // target is simply resolved now.
            "cat <<'EOF' > file.rs\nfn main() {}\nEOF",
        ] {
            assert!(
                check_command(cmd, &rules, None).is_none(),
                "resolvable write should be allowed: {cmd}",
            );
        }
    }

    #[test]
    fn opaque_redirects_are_denied_with_teaching() {
        let rules = basic_rules();
        for cmd in [
            "echo hi > $F",
            "make test > ${OUT:-default}",
            "echo x > ~user/f",
        ] {
            let denial = check_command(cmd, &rules, None).expect("opaque write denied");
            assert_eq!(denial.reason, DenialReason::OpaqueWrite, "{cmd}");
            assert!(denial.message.is_some(), "teaching message present: {cmd}");
        }
    }

    #[test]
    fn opaque_write_denial_message_is_the_teaching_text() {
        let rules = basic_rules();
        let denial = check_command("echo hi > $F", &rules, None).expect("denied");
        let msg = format_denial(&denial.command, &rules, &denial, None, None);
        assert!(msg.contains("$F"), "names the construct: {msg}");
        assert!(
            msg.contains("Bind it"),
            "teaches the resolvable form: {msg}"
        );
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
    fn tee_file_operand_handled() {
        // `tee` is absent from the default pipeline, so a `tee <file>` write
        // vector is denied (NotAllowed) rather than waved through.
        let rules = basic_rules();
        assert!(check_command("make test | tee src/x.rs", &rules, None).is_some());
    }

    // ── Ticket-03 redirect-guard cases (parse-driven) ───────────────

    #[test]
    fn quoted_redirect_echo_allowed() {
        // Bug 33c: the `>` lives inside a quoted argument, so the faithful
        // parse records no redirect — `echo "a > b"` is allowed (the false
        // positive the byte scanner produced is gone).
        let rules = basic_rules();
        assert!(check_command(r#"echo "a > b""#, &rules, None).is_none());
    }

    #[test]
    fn echo_real_redirect_resolves() {
        // The real redirect is a resolved, recorded write now (ws38).
        let rules = basic_rules();
        assert!(check_command("echo hi > out.txt", &rules, None).is_none());
    }

    #[test]
    fn echo_redirect_to_device_sink_allowed() {
        let rules = basic_rules();
        assert!(check_command("echo hi > /dev/null", &rules, None).is_none());
    }

    #[test]
    fn fd_dup_and_close_allowed() {
        // `2>&1` duplicates a descriptor and `>&-` closes one — neither writes
        // a file, so both pass through the build tool unflagged.
        let rules = basic_rules();
        assert!(check_command("make test 2>&1", &rules, None).is_none());
        assert!(check_command("make test >&-", &rules, None).is_none());
    }

    #[test]
    fn opaque_write_inside_substitution_denied() {
        // The write lives inside a command substitution; the resolver's
        // substitution recursion still classifies it. A resolvable nested
        // write is allowed; an opaque one denies at any depth.
        let rules = basic_rules();
        assert!(
            check_command("echo $(git status > stamp)", &rules, None).is_none(),
            "resolvable nested write allowed",
        );
        let denial = check_command("echo $(git status > $STAMP)", &rules, None)
            .expect("opaque nested write denied");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
        let denial = check_command("echo $(echo $(git log > $DEEP))", &rules, None)
            .expect("opaque write two substitution levels deep must still deny");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
    }

    #[test]
    fn dup_out_to_file_target_resolves_or_denies() {
        // `>&file` (a non-fd target) is the zsh/bash combined-stream file
        // write, not a descriptor duplication — a real write, resolved like
        // any other (§8a fail-closed shape preserved for opaque targets).
        let rules = basic_rules();
        assert!(check_command("make test >&out.log", &rules, None).is_none());
        assert_eq!(
            check_command("make test >&$LOG", &rules, None)
                .expect("opaque dup-out target denied")
                .reason,
            DenialReason::OpaqueWrite,
        );
    }

    #[test]
    fn multios_targets_all_resolved() {
        // MULTIOS: `> a > b` writes *both* targets — each resolves; one opaque
        // target anywhere denies the line.
        let rules = basic_rules();
        assert!(check_command("echo hi > a > b", &rules, None).is_none());
        assert_eq!(
            check_command("echo hi > a > $B", &rules, None)
                .expect("one opaque multios target denies")
                .reason,
            DenialReason::OpaqueWrite,
        );
    }

    #[test]
    fn variable_redirect_target_fails_closed() {
        // `> $f` is an unverifiable target unless bound in the same command
        // line — the resolver denies (complete-or-deny), and a same-line
        // binding resolves it.
        let rules = basic_rules();
        let denial =
            check_command("echo hi > $f", &rules, None).expect("variable target fails closed");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
        assert!(
            check_command("f=out.txt; echo hi > $f", &rules, None).is_none(),
            "same-line binding resolves the target",
        );
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
        match analyze_catenary_command(cmd, None) {
            CatenaryAction::Deny(m) => m,
            _ => String::new(),
        }
    }

    // ---- Accept table ----

    #[test]
    fn matcher_accepts_bare_search() {
        assert_eq!(
            analyze_catenary_command("catenary grep \"p\" src", None),
            CatenaryAction::Allow { has_foreign: false },
        );
        assert_eq!(
            analyze_catenary_command("catenary glob src", None),
            CatenaryAction::Allow { has_foreign: false },
        );
    }

    #[test]
    fn matcher_accepts_bare_correlated_and_lifecycle() {
        for cmd in [
            "catenary roots",
            "catenary roots ls",
            "catenary pin /tmp/p",
            "catenary unpin /tmp/p",
            "catenary primer",
            "catenary commands",
        ] {
            assert_eq!(
                analyze_catenary_command(cmd, None),
                CatenaryAction::Allow { has_foreign: false },
                "{cmd} should be a bare allow",
            );
        }
    }

    #[test]
    fn pin_unpin_and_bare_roots_recognized_as_lifecycle() {
        // misc 146: `pin`/`unpin` join the bare-only lifecycle class (like
        // `roots`), and bare `catenary roots` (plus the `ls` alias) lists.
        assert_eq!(
            recognize_catenary_sub(&["pin", "/p"]),
            Recog::Agent(Sub::Pin)
        );
        assert_eq!(
            recognize_catenary_sub(&["unpin", "/p"]),
            Recog::Agent(Sub::Unpin),
        );
        assert_eq!(recognize_catenary_sub(&["roots"]), Recog::Agent(Sub::Roots));
        assert_eq!(
            recognize_catenary_sub(&["roots", "ls"]),
            Recog::Agent(Sub::Roots),
        );
        for sub in [Sub::Pin, Sub::Unpin, Sub::Roots] {
            assert_eq!(sub.class(), CatenaryClass::Lifecycle);
        }
    }

    #[test]
    fn roots_add_rm_retired_to_pin_unpin() {
        // The old `roots add`/`roots rm` spellings retire with a rename redirect
        // in every form — never routed, never a generic "unknown command".
        for cmd in [
            "catenary roots add /tmp/p",
            "catenary roots rm /tmp/p",
            "/usr/local/bin/catenary roots add /tmp/p",
            "catenary roots rm /tmp/p | head",
            "cd src && catenary roots add /tmp/p",
        ] {
            let msg = deny_text(cmd);
            assert!(
                msg.contains("catenary pin") && msg.contains("catenary unpin"),
                "{cmd} should redirect to pin/unpin, got: {msg}",
            );
        }
    }

    #[test]
    fn pin_unpin_are_bare_only_lifecycle() {
        // Chained or piped-out → bare-only violation (they take the daemon
        // handoff, like `roots`).
        assert!(
            matches!(
                analyze_catenary_command("cd /repo && catenary pin .", None),
                CatenaryAction::Deny(_),
            ),
            "pin must be the sole command",
        );
        assert!(
            matches!(
                analyze_catenary_command("catenary unpin /p | tee log", None),
                CatenaryAction::Deny(_),
            ),
            "unpin must not pipe out",
        );
    }

    #[test]
    fn claim_recognized_as_bare_only_lifecycle() {
        // Root-ownership stage 2: `catenary claim <root>` is agent-invocable,
        // maps to the dedicated Claim action (the hook stages, the CLI drains),
        // and is bare-only (it mutates the on-disk lock, so it takes the handoff
        // and must be the sole command).
        assert_eq!(
            recognize_catenary_sub(&["claim", "/repo"]),
            Recog::Agent(Sub::Claim),
        );
        assert_eq!(Sub::Claim.class(), CatenaryClass::Lifecycle);
        assert_eq!(
            analyze_catenary_command("catenary claim /repo", None),
            CatenaryAction::Claim,
            "a bare claim maps to the Claim action",
        );
        assert!(
            matches!(
                analyze_catenary_command("cd /repo && catenary claim .", None),
                CatenaryAction::Deny(_),
            ),
            "claim must be the sole command",
        );
        assert!(
            matches!(
                analyze_catenary_command("catenary claim /repo | tee log", None),
                CatenaryAction::Deny(_),
            ),
            "claim must not pipe out",
        );
    }

    #[test]
    fn worktree_ls_is_search_class_pipes_and_chains() {
        // `catenary worktree ls` is Search-class (misc 151): pipe-friendly,
        // chainable, no isolation.
        assert_eq!(
            analyze_catenary_command("catenary worktree ls", None),
            CatenaryAction::Allow { has_foreign: false },
        );
        assert_eq!(
            analyze_catenary_command("catenary worktree ls | grep feat", None),
            CatenaryAction::Allow { has_foreign: true },
            "worktree ls pipes into a downstream filter",
        );
    }

    #[test]
    fn worktree_add_rm_are_bare_only_lifecycle() {
        // `add`/`rm` mutate the on-disk worktree set: bare-only lifecycle. With
        // no declared client (or one whose hook set lacks WorktreeCreate, below)
        // `add` stays a plain lifecycle verb — the dispatch deny (misc 177) is
        // client-keyed.
        for cmd in [
            "catenary worktree add feature/auth",
            "catenary worktree rm /some/path",
        ] {
            assert_eq!(
                analyze_catenary_command(cmd, None),
                CatenaryAction::Allow { has_foreign: false },
                "{cmd} should be a bare allow",
            );
        }
        // Chained or piped → bare-only violation.
        assert!(
            matches!(
                analyze_catenary_command("cd /repo && catenary worktree add topic", None),
                CatenaryAction::Deny(_),
            ),
            "worktree add must be the sole command",
        );
        assert!(
            matches!(
                analyze_catenary_command("catenary worktree rm /p | tee log", None),
                CatenaryAction::Deny(_),
            ),
            "worktree rm must not pipe out",
        );
    }

    #[test]
    fn worktree_add_denied_with_dispatch_teaching_for_worktree_create_clients() {
        // misc 177: on a client whose installed hook set registers
        // WorktreeCreate (today Claude Code), agent-side `worktree add` is
        // denied in ANY form — bare, path-prefixed, chained, piped, wrapped —
        // and the teaching always names the sanctioned dispatch flow, never a
        // generic bare-only complaint.
        for cmd in [
            "catenary worktree add feature/auth",
            "/usr/local/bin/catenary worktree add topic",
            "cd /repo && catenary worktree add topic",
            "catenary worktree add topic | tee log",
            "echo $(catenary worktree add topic)",
        ] {
            let msg = match analyze_catenary_command(cmd, Some(HostFormat::Claude)) {
                CatenaryAction::Deny(m) => m,
                _ => String::new(),
            };
            assert!(
                !msg.is_empty(),
                "{cmd} must be denied for a WorktreeCreate client",
            );
            assert!(
                msg.contains("isolation: \"worktree\"")
                    && msg.contains("WorktreeCreate")
                    && msg.contains("git merge --squash")
                    && msg.contains("worktree rm"),
                "{cmd} must teach the dispatch flow and the git-native landing, got: {msg}",
            );
        }
    }

    #[test]
    fn worktree_cleanup_verbs_survive_the_dispatch_deny() {
        // The deny is surgical (misc 177): `rm` is the sanctioned cleanup path
        // (WorktreeRemove never fires upstream) and `ls` is a read — both stay
        // available to WorktreeCreate clients. The retired `land`/`diff` stubs
        // (wf-03) stay reachable too, so their teaching can print; their
        // entries delete with the stubs in a later release.
        for cmd in [
            "catenary worktree rm /some/path",
            "catenary worktree land /wt",
            "catenary worktree ls",
            "catenary worktree diff /wt",
        ] {
            assert_eq!(
                analyze_catenary_command(cmd, Some(HostFormat::Claude)),
                CatenaryAction::Allow { has_foreign: false },
                "{cmd} must stay allowed for a WorktreeCreate client",
            );
        }
    }

    #[test]
    fn worktree_add_stays_lifecycle_for_clients_without_worktree_create() {
        // Only a hook set that registers WorktreeCreate keys the deny —
        // Antigravity and OpenCode keep the plain bare-only lifecycle verb (a
        // hand-run add is their only worktree path).
        for client in [HostFormat::Antigravity, HostFormat::OpenCode] {
            assert_eq!(
                analyze_catenary_command("catenary worktree add feature/auth", Some(client)),
                CatenaryAction::Allow { has_foreign: false },
                "{client:?} must keep worktree add until its hook set carries WorktreeCreate",
            );
        }
    }

    #[test]
    fn worktree_rm_force_denied_with_lifecycle_teaching_for_worktree_create_clients() {
        // misc 188: on a client whose installed hook set registers
        // WorktreeCreate (today Claude Code), agent-side `worktree rm --force`
        // — the dirty-discard lever — is denied in ANY form (bare, path-first,
        // flag-first, path-prefixed, chained, piped, wrapped), and the teaching
        // always names the worktree lifecycle, never a generic complaint.
        for cmd in [
            "catenary worktree rm --force /some/path",
            "catenary worktree rm /some/path --force",
            "/usr/local/bin/catenary worktree rm --force /wt",
            "cd /repo && catenary worktree rm --force /wt",
            "catenary worktree rm --force /wt | tee log",
            "echo $(catenary worktree rm --force /wt)",
        ] {
            let msg = match analyze_catenary_command(cmd, Some(HostFormat::Claude)) {
                CatenaryAction::Deny(m) => m,
                _ => String::new(),
            };
            assert!(
                !msg.is_empty(),
                "{cmd} must be denied for a WorktreeCreate client",
            );
            assert!(
                msg.contains("--force")
                    && msg.contains("maintainer's lever")
                    && msg.contains("git diff main...")
                    && msg.contains("git merge --squash")
                    && msg.contains("worktree rm"),
                "{cmd} must teach the worktree lifecycle and the maintainer's lever, got: {msg}",
            );
        }
    }

    #[test]
    fn bare_worktree_rm_survives_the_force_deny_for_worktree_create_clients() {
        // The deny is surgical (misc 188): only the explicit `--force` is
        // bounced. Bare `worktree rm` (which refuses a dirty worktree itself,
        // misc 158) stays the sanctioned clean-disposal verb for WorktreeCreate
        // clients — a token merely *containing* "force" doesn't trip it.
        for cmd in [
            "catenary worktree rm /some/path",
            "catenary worktree rm /forced/path",
        ] {
            assert_eq!(
                analyze_catenary_command(cmd, Some(HostFormat::Claude)),
                CatenaryAction::Allow { has_foreign: false },
                "{cmd} must stay allowed for a WorktreeCreate client",
            );
        }
    }

    #[test]
    fn worktree_rm_force_stays_lifecycle_for_clients_without_worktree_create() {
        // Only a hook set that registers WorktreeCreate keys the deny — the
        // host-keyed path (Antigravity/OpenCode, and the daemon-side `None`
        // classifier) keeps `worktree rm --force` a plain bare-only lifecycle
        // verb, exactly as the misc-177 add deny stays client-keyed.
        assert_eq!(
            analyze_catenary_command("catenary worktree rm --force /wt", None),
            CatenaryAction::Allow { has_foreign: false },
            "no declared client keeps worktree rm --force a plain lifecycle verb",
        );
        for client in [HostFormat::Antigravity, HostFormat::OpenCode] {
            assert_eq!(
                analyze_catenary_command("catenary worktree rm --force /wt", Some(client)),
                CatenaryAction::Allow { has_foreign: false },
                "{client:?} must keep worktree rm --force until its hook set carries WorktreeCreate",
            );
        }
    }

    #[test]
    fn worktree_diff_is_search_class_pipes_and_chains() {
        // `catenary worktree diff` retired to a teaching stub (wf-03) but keeps
        // its Search class during the transition, so every old invocation shape
        // reaches the stub (which prints the git-native flow and exits 2).
        assert_eq!(
            analyze_catenary_command("catenary worktree diff /wt", None),
            CatenaryAction::Allow { has_foreign: false },
        );
        assert_eq!(
            analyze_catenary_command("catenary worktree diff /wt | git apply -", None),
            CatenaryAction::Allow { has_foreign: true },
            "worktree diff pipes into a downstream git apply",
        );
        assert_eq!(
            analyze_catenary_command("catenary worktree diff /wt --name-only | sort", None),
            CatenaryAction::Allow { has_foreign: true },
            "--name-only pipes into a downstream filter",
        );
    }

    #[test]
    fn worktree_land_is_bare_only_lifecycle() {
        // `catenary worktree land` retired to a teaching stub (wf-03) but keeps
        // its bare-only lifecycle class during the transition, so a bare
        // invocation reaches the stub (which prints the git-native flow and
        // exits 2).
        assert_eq!(
            analyze_catenary_command("catenary worktree land /wt", None),
            CatenaryAction::Allow { has_foreign: false },
            "a bare land is allowed",
        );
        assert_eq!(
            analyze_catenary_command("catenary worktree land /wt --keep", None),
            CatenaryAction::Allow { has_foreign: false },
            "--keep is still a bare allow",
        );
        // Chained or piped → bare-only violation.
        assert!(
            matches!(
                analyze_catenary_command("cd /repo && catenary worktree land /wt", None),
                CatenaryAction::Deny(_),
            ),
            "worktree land must be the sole command",
        );
        assert!(
            matches!(
                analyze_catenary_command("catenary worktree land /wt | tee log", None),
                CatenaryAction::Deny(_),
            ),
            "worktree land must not pipe out",
        );
    }

    #[test]
    fn matcher_accepts_top_level_version_and_help() {
        // bug 22: clap's global `--version`/`-V` and `--help`/`-h` carry no
        // subcommand, so the canonical-form matcher must admit the subcommand-
        // less forms as a pure read (no handoff, no isolation/redirect concern).
        // The `version` subcommand is the same read plus a stateless daemon-
        // version query — same class.
        for cmd in [
            "catenary --version",
            "catenary -V",
            "catenary --help",
            "catenary -h",
            "catenary version",
        ] {
            assert_eq!(
                analyze_catenary_command(cmd, None),
                CatenaryAction::Allow { has_foreign: false },
                "{cmd} should be a bare global-read allow",
            );
        }
    }

    #[test]
    fn matcher_denies_unknown_top_level_flag() {
        // bug 22 scope: only `--version`/`--help` are admitted. An unknown
        // top-level flag with no subcommand stays fail-closed.
        assert!(
            deny_text("catenary --frobnicate").contains("isn't a recognized"),
            "an unknown top-level flag must still deny",
        );
    }

    #[test]
    fn matcher_denies_global_flag_with_extra_arg() {
        // misc 142: the subcommand-less informational flags are admitted only
        // as the *sole* argument. A flag carrying an extra arg (or a second
        // flag) is not "exactly one global informational flag", so it stays
        // fail-closed — only `catenary version` (the subcommand) admits a
        // richer form.
        for cmd in [
            "catenary --version extra",
            "catenary -V extra",
            "catenary --help topic",
            "catenary -h topic",
            "catenary --help --version",
        ] {
            assert!(
                deny_text(cmd).contains("isn't a recognized"),
                "{cmd} must deny — a global flag is admitted only as the sole argument",
            );
        }
    }

    #[test]
    fn matcher_subcommand_help_unaffected() {
        // bug 22 scope: subcommand-scoped help still resolves via the
        // subcommand arm (the global-read arm sits after them), so
        // `catenary grep --help` is a search allow, not the global read.
        assert_eq!(
            analyze_catenary_command("catenary grep --help", None),
            CatenaryAction::Allow { has_foreign: false },
        );
    }

    #[test]
    fn matcher_denies_commands_pipe_and_chain() {
        // `catenary commands` is lifecycle/bare-only — it takes a daemon handoff,
        // so a pipe or chain is a bare-only violation.
        assert!(
            matches!(
                analyze_catenary_command("catenary commands | grep git", None),
                CatenaryAction::Deny(_),
            ),
            "piping `catenary commands` should deny",
        );
        assert!(deny_text("cd src && catenary commands").contains("its own"));
    }

    #[test]
    fn matcher_redirects_retired_sed() {
        // `catenary sed` is retired (ws38 06) — every form gets the teaching
        // redirect to native `sed -i`, in a bare invocation or a chain.
        for cmd in [
            "catenary sed --in-place a b src",
            "catenary sed a b src",
            "cd src && catenary sed --in-place a b .",
            "catenary sed a b > preview.diff",
        ] {
            let msg = deny_text(cmd);
            assert!(
                msg.contains("`catenary sed` is retired") && msg.contains("sed -i"),
                "`{cmd}` should teach native sed -i, got: {msg}",
            );
        }
    }

    #[test]
    fn matcher_routes_editing_lifecycle() {
        assert_eq!(
            analyze_catenary_command("catenary editing start", None),
            CatenaryAction::EditingStart,
        );
        assert_eq!(
            analyze_catenary_command("catenary diagnostics", None),
            CatenaryAction::Diagnostics,
        );
        assert_eq!(
            analyze_catenary_command("/usr/local/bin/catenary diagnostics", None),
            CatenaryAction::Diagnostics,
        );
        assert_eq!(
            analyze_catenary_command("DEBUG=1 catenary editing start", None),
            CatenaryAction::EditingStart,
        );
    }

    #[test]
    fn diagnostics_deny_precedes_drain() {
        // Ordering constraint (ticket 11): a piped `catenary diagnostics` must
        // classify as `Deny`, never `Diagnostics`. `run_pre_tool` dispatches
        // `Deny` (print + return) *before* `Diagnostics` (which runs the hook-side
        // owner gate and allows the serve). Root-ownership stage 3 retired the
        // two-phase prepare-drain — the serve reads the durable ledger — so the
        // surviving guard is that a piped form denies (bare-only) and never
        // reaches the serve. The bare form is the only one that routes.
        assert!(
            matches!(
                analyze_catenary_command("catenary diagnostics | head", None),
                CatenaryAction::Deny(_)
            ),
            "piped diagnostics must deny before the prepare drains the set",
        );
        assert!(matches!(
            analyze_catenary_command("catenary diagnostics > out.txt", None),
            CatenaryAction::Deny(_)
        ));
        assert!(matches!(
            analyze_catenary_command("catenary diagnostics && make test", None),
            CatenaryAction::Deny(_)
        ));
        // The bare form is the only one that reaches the drain.
        assert_eq!(
            analyze_catenary_command("catenary diagnostics", None),
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
            analyze_catenary_command("cd src && catenary grep p", None),
            CatenaryAction::Allow { has_foreign: true },
        );
        assert_eq!(
            analyze_catenary_command("catenary grep a && catenary grep b", None),
            CatenaryAction::Allow { has_foreign: false },
        );
        assert_eq!(
            analyze_catenary_command("catenary glob x ; catenary glob y", None),
            CatenaryAction::Allow { has_foreign: false },
        );
    }

    #[test]
    fn matcher_accepts_arg_substitution() {
        // `$VAR` is not a substitution — bare allow.
        assert_eq!(
            analyze_catenary_command("catenary grep \"$PAT\"", None),
            CatenaryAction::Allow { has_foreign: false },
        );
        // `$(cmd)` inside an arg is permitted; the inner command is flagged for
        // regime-2 allowlist validation (`has_foreign: true`), not denied here.
        assert_eq!(
            analyze_catenary_command("catenary grep \"$(rg-config)\"", None),
            CatenaryAction::Allow { has_foreign: true },
        );
    }

    #[test]
    fn matcher_path_prefix_and_quoted_literal() {
        // Path prefix + a literal "catenary" inside the pattern: positional
        // tokenization still resolves the subcommand to grep.
        assert_eq!(
            analyze_catenary_command(r#"/opt/catenary/bin/catenary grep "catenary" src"#, None),
            CatenaryAction::Allow { has_foreign: false },
        );
    }

    // ---- Deny table: output ownership (both classes) ----

    #[test]
    fn matcher_allows_search_pipe_out() {
        // Ticket 07 / decision 025: search output is complete and client-owned,
        // so piping it downstream is now allowed — the out-pipe denial is retired
        // for `grep`/`glob`. (Downstream foreign segments are allowlist-checked
        // separately, hence `has_foreign: true`.)
        for cmd in [
            "catenary grep p | head",
            "catenary grep p | wc -l",
            "catenary glob src | tail",
            "catenary grep p | grep foo",
            // `2>&1 |` around a search command is fine too — stderr no longer
            // carries a receipt (ticket 06).
            "catenary grep p 2>&1 | head",
        ] {
            assert_eq!(
                analyze_catenary_command(cmd, None),
                CatenaryAction::Allow { has_foreign: true },
                "{cmd} should be an allow with foreign downstream",
            );
        }
    }

    #[test]
    fn matcher_admits_query_as_search_class() {
        // Misc 149 (maintainer ruling: "pure observability"): `catenary query`
        // is Search-class — read-only telemetry with complete, client-owned
        // output — so it admits bare, chains, and pipes out freely.
        assert_eq!(recognize_catenary_sub(&["query"]), Recog::Agent(Sub::Query));
        assert_eq!(
            analyze_catenary_command("catenary query --kind hook --search worktree-create", None),
            CatenaryAction::Allow { has_foreign: false },
        );
        assert_eq!(
            analyze_catenary_command("catenary query --kind hook | head", None),
            CatenaryAction::Allow { has_foreign: true },
        );
        // Unlike grep/glob, query reads no stdin — a pipe INTO it teaches
        // instead of silently no-opping.
        assert!(deny_text("cat log.jsonl | catenary query").contains("takes no stdin"));
    }

    #[test]
    fn matcher_allows_search_pipe_in() {
        // Ticket 05: the bug-19 / ADR-013 post-pipe guard is retired. `catenary
        // grep`/`glob` read stdin (ticket 04), so a downstream pipe position is a
        // valid invocation — the catenary matcher no longer denies it (the
        // upstream foreign segment is allowlist-checked separately, hence
        // `has_foreign: true`).
        assert_eq!(
            analyze_catenary_command("chezmoi managed | catenary grep p", None),
            CatenaryAction::Allow { has_foreign: true },
        );
        assert_eq!(
            analyze_catenary_command("ls | catenary glob src", None),
            CatenaryAction::Allow { has_foreign: true },
        );
        assert_eq!(
            analyze_catenary_command("cat src/main.rs | catenary grep p", None),
            CatenaryAction::Allow { has_foreign: true },
        );
    }

    #[test]
    fn matcher_denies_pipe_in_for_no_stdin_classes() {
        // The correlated/lifecycle classes take no stdin and stay bare-only, so a
        // downstream pipe still denies — the post-pipe guard is retired only for
        // the Search class.
        assert!(
            matches!(
                analyze_catenary_command("git log | catenary diagnostics", None),
                CatenaryAction::Deny(_),
            ),
            "piped-in diagnostics must still deny",
        );
        assert!(
            deny_text("echo x | catenary pin /tmp/p").contains("stdin"),
            "piped-in lifecycle command must still deny on stdin",
        );
    }

    #[test]
    fn matcher_allows_search_redirect() {
        // Ticket 07 / decision 025: search output is complete and client-owned,
        // so a file redirect is now allowed.
        assert_eq!(
            analyze_catenary_command("catenary grep p > out.txt", None),
            CatenaryAction::Allow { has_foreign: false },
        );
        assert_eq!(
            analyze_catenary_command("catenary glob src > out.txt", None),
            CatenaryAction::Allow { has_foreign: false },
        );
    }

    #[test]
    fn matcher_denies_handoff_redirect_no_stale_hints() {
        // A handoff-carrying command stays bare-only: a redirect still denies,
        // but the message teaches the bare form and drops the retired volume
        // hints (spill file / runtime-dir report / `--page`).
        let diag = deny_text("catenary diagnostics > out.txt");
        assert!(
            diag.contains("redirect") && diag.contains("bare"),
            "must teach the bare form: {diag}",
        );
        for stale in ["runtime-dir", "--page", "spill", "overflow valve"] {
            assert!(
                !diag.contains(stale),
                "stale hint `{stale}` must be gone: {diag}",
            );
        }
    }

    #[test]
    fn matcher_denies_background() {
        // The message names the offending subcommand via `Sub::label`; pin both
        // the "background" reason and the embedded canonical label so a
        // `Sub::label` substitution (`"grep" → "xyzzy"/""`) can't slip past.
        let grep = deny_text("catenary grep p &");
        assert!(grep.contains("background"), "got: {grep}");
        assert!(
            grep.contains("catenary grep"),
            "background denial must name `catenary grep`, got: {grep}",
        );
        let start = deny_text("catenary editing start &");
        assert!(start.contains("background"), "got: {start}");
        assert!(
            start.contains("catenary editing start"),
            "background denial must name `catenary editing start`, got: {start}",
        );
    }

    #[test]
    fn matcher_denies_substitution_wrap() {
        // The substitution-capture message names the wrapped subcommand via
        // `Sub::label` — assert it so a `Sub::label` substitution is caught.
        let grep = deny_text("$(catenary grep p)");
        assert!(grep.contains("capture"), "got: {grep}");
        assert!(
            grep.contains("catenary grep"),
            "capture denial must name `catenary grep`, got: {grep}",
        );
        let glob = deny_text("echo `catenary glob src`");
        assert!(glob.contains("capture"), "got: {glob}");
        assert!(
            glob.contains("catenary glob"),
            "capture denial must name `catenary glob`, got: {glob}",
        );
    }

    // ---- Deny table: bare-only (correlated/lifecycle) ----

    #[test]
    fn matcher_denies_correlated_prefix_and_chain() {
        // The unified bare-only denial names the isolation-needing command
        // (`as its own command`) and embeds its `Sub::label`.
        let diag_prefix = deny_text("cd src && catenary diagnostics");
        assert!(
            diag_prefix.contains("as its own command"),
            "got: {diag_prefix}"
        );
        assert!(
            diag_prefix.contains("catenary diagnostics"),
            "got: {diag_prefix}"
        );

        let diag_chain = deny_text("catenary diagnostics && make test");
        assert!(
            diag_chain.contains("as its own command"),
            "got: {diag_chain}"
        );
        assert!(
            diag_chain.contains("catenary diagnostics"),
            "got: {diag_chain}"
        );
    }

    #[test]
    fn matcher_denies_two_correlated_in_one_call() {
        // Two correlated invocations chained: the unified bare-only denial names
        // the isolation-needing command (`diagnostics`).
        let msg = deny_text("catenary diagnostics && catenary diagnostics");
        assert!(msg.contains("as its own command"), "got: {msg}");
        assert!(msg.contains("catenary diagnostics"), "got: {msg}");
    }

    #[test]
    fn matcher_denies_search_mixed_with_correlated() {
        // grep is unrestricted, but diagnostics is not bare → deny. The denial
        // must name the isolation-needing command (`diagnostics`), not the
        // freely-chaining `grep` that happens to be `subs.first()`.
        let msg = deny_text("catenary grep p && catenary diagnostics");
        assert!(msg.contains("as its own command"), "got: {msg}");
        assert!(msg.contains("catenary diagnostics"), "got: {msg}");
        assert!(
            !msg.contains("catenary grep"),
            "must name diagnostics, not grep: {msg}"
        );
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

    /// Regression pin (misc 123 / feedback 08 finding 3): `catenary stop` is a
    /// host-only daemon-lifecycle command — an agent must never be able to turn
    /// the daemon off (and disrupt every other session). The stop-confirmation
    /// UX (a human-TTY-only prompt, `--force` to skip) does NOT open an agent
    /// path: `recognize_catenary_sub` keeps classifying `stop` as `NotAgent`,
    /// and the `--force` flag does not change that. Do not weaken without a
    /// maintainer ruling.
    #[test]
    fn recognize_catenary_stop_stays_not_agent() {
        assert_eq!(recognize_catenary_sub(&["stop"]), Recog::NotAgent);
        // Trailing args (the new `--force`) don't reclassify it.
        assert_eq!(
            recognize_catenary_sub(&["stop", "--force"]),
            Recog::NotAgent,
        );
        // And the full pipeline still denies both forms for the agent surface.
        assert!(deny_text("catenary stop").contains("host CLI hooks"));
        assert!(deny_text("catenary stop --force").contains("host CLI hooks"));
    }

    /// `catenary start` (bug 80, leg 2) is a daemon-lifecycle verb like `stop`,
    /// not an agent surface: `recognize_catenary_sub` classifies it `NotAgent`
    /// (so it is not an unknown-subcommand denial), and the full agent pipeline
    /// still routes it away from the agent surface.
    #[test]
    fn recognize_catenary_start_stays_not_agent() {
        assert_eq!(recognize_catenary_sub(&["start"]), Recog::NotAgent);
        assert!(deny_text("catenary start").contains("host CLI hooks"));
    }

    /// `catenary restart` / `catenary quit` (pulse 04) are daemon-lifecycle
    /// verbs like `start`/`stop`: host-CLI-only, never agent-invocable — an
    /// agent must not bounce the daemon or end other sessions' bridges. They
    /// classify `NotAgent` (accurate host-CLI teaching, not an
    /// unknown-subcommand denial), and they stay OUT of the agent-available
    /// surface listing.
    #[test]
    fn recognize_catenary_restart_and_quit_stay_not_agent() {
        assert_eq!(recognize_catenary_sub(&["restart"]), Recog::NotAgent);
        assert_eq!(recognize_catenary_sub(&["quit"]), Recog::NotAgent);
        assert_eq!(
            recognize_catenary_sub(&["quit", "--force"]),
            Recog::NotAgent,
        );
        assert!(deny_text("catenary restart").contains("host CLI hooks"));
        assert!(deny_text("catenary quit").contains("host CLI hooks"));
        assert!(deny_text("catenary quit --force").contains("host CLI hooks"));
        // The agent-available listing in the denial must not name the
        // host-only lifecycle verbs.
        for verb in ["`restart`", "`quit`", "`start`", "`stop`"] {
            assert!(
                !CATENARY_SURFACE.contains(verb),
                "{verb} must stay out of the agent-available surface listing",
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
                analyze_catenary_command(cmd, None),
                CatenaryAction::NotCatenary,
                "{cmd} has no catenary command",
            );
        }
    }

    // ---- bugs/16 regression ----

    #[test]
    fn matcher_bugs16_piped_lifecycle_is_clear_pipe_deny() {
        // A piped lifecycle command yields a clear pipe-deny, not a routed
        // action and not (downstream) the boundary block. The message teaches the
        // bare form (no retired volume hints).
        let start = deny_text("catenary editing start | head");
        assert!(start.contains("bare"), "got: {start}");
        let diag = deny_text("catenary diagnostics | head");
        assert!(diag.contains("bare"), "got: {diag}");
        assert!(!diag.contains("runtime-dir"), "no stale hint: {diag}");
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

    #[test]
    fn check_and_resolve_surfaces_catenary_write_set() {
        let rules = basic_rules();
        // A catenary-only redirect resolves its target (folded-in edge,
        // ws38 ticket 02): allowed, and the write-set carries the target so
        // the daemon can attribute it.
        let writes = check_and_resolve_command("catenary grep p > out.txt", &rules, None)
            .expect("resolvable catenary redirect allowed");
        assert!(
            writes.writes.iter().any(|p| p.ends_with("out.txt")),
            "resolved write-set should record the redirect target, got {:?}",
            writes.writes,
        );
        // An opaque catenary redirect denies with the resolver's teaching
        // message (complete-or-deny).
        let denial = check_and_resolve_command("catenary grep p > $OUT", &rules, None)
            .expect_err("opaque catenary redirect denied");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
        // A plain catenary read records nothing.
        let writes = check_and_resolve_command("catenary grep p src", &rules, None)
            .expect("plain catenary read allowed");
        assert!(writes.writes.is_empty(), "a read records no writes");
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
        match analyze_catenary_command(cmd, None) {
            CatenaryAction::Deny(_) => Outcome::DenyCatenary,
            // Every allowed catenary line resolves its write-set through the
            // same path (ws38 ticket 02 folded-in edge): a catenary-only
            // redirect (`catenary grep p > f`) is now resolve-gated, so an
            // opaque target denies while a literal one still allows. `catenary`
            // segments are skipped by the allowlist walk, so a search chain's
            // foreign part is still validated.
            CatenaryAction::NotCatenary | CatenaryAction::Allow { .. } => foreign(cmd),
            CatenaryAction::EditingStart | CatenaryAction::Diagnostics | CatenaryAction::Claim => {
                Outcome::Allow
            }
        }
    }

    #[test]
    fn composition_table() {
        use DenialReason::{NotAllowed, OpaqueWrite, PipelinePosition};
        use Outcome::{Allow, DenyCatenary, DenyForeign};
        let rules = recommended_rules();
        let cases: &[(&str, Outcome)] = &[
            // ── Foreign allowlist + resolve-or-deny writes (ws38) + reads ──
            ("git status", Allow),
            ("cat src/main.rs", Allow),
            // Resolvable redirects are recorded writes, allowed (ws38 01).
            ("git status > out.txt", Allow),
            ("cat foo > bar.rs", Allow),
            ("cat <<'EOF' > f.rs\nfn x(){}\nEOF", Allow),
            // Opaque write targets deny with a teaching message.
            ("git status > $OUT", DenyForeign(OpaqueWrite)),
            ("echo x > $(cat name)", DenyForeign(OpaqueWrite)),
            ("make test 2>&1", Allow),
            ("make test > /dev/null", Allow),
            // ── awk out of the pipeline; sed writes are resolver-checked ──
            // `sed` is allowlisted (ws38 06), but its `w` write is unattributable,
            // so the resolver denies with a construct-naming teaching message.
            ("git log | sed -n 'w /tmp/x'", DenyForeign(OpaqueWrite)),
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
            // pipe-out + redirect retired for search (ticket 07 / decision 025):
            // complete, client-owned output pipes/redirects as freely as `grep`.
            ("catenary grep p | head", Allow),
            ("catenary grep p | wc -l", Allow),
            ("catenary grep p > out.txt", Allow),
            ("catenary grep p 2>&1 | head", Allow),
            ("catenary sed a b > preview.diff", DenyCatenary), // retired → native sed
            // Catenary-only redirects are resolve-gated (ws38 ticket 02
            // folded-in edge): a literal target attributes and allows, an
            // opaque target denies with the resolver's teaching message.
            ("catenary grep p > $OUT", DenyForeign(OpaqueWrite)),
            // pipe-in retired (ticket 05): search reads stdin now — a downstream
            // pipe is valid, so only the *upstream* segment is judged. Allowlisted
            // upstream → the whole call runs; an unlisted upstream denies on its
            // own name (not on the pipe).
            ("cat src/main.rs | catenary grep p", Allow),
            ("chezmoi managed | catenary grep p", DenyForeign(NotAllowed)), // upstream unlisted
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
            ("catenary sed --in-place a b src", DenyCatenary), // retired → native sed
            ("catenary editing stop", DenyCatenary),         // retired → diagnostics
            ("catenary frobnicate", DenyCatenary),
            ("catenary daemon", DenyCatenary), // not agent-invocable
            // ── catenary regime 1: top-level global reads (bug 22) ──
            ("catenary --version", Allow),
            ("catenary -V", Allow),
            ("catenary --help", Allow),
            ("catenary -h", Allow),
            ("catenary version", Allow), // subcommand form: CLI + daemon versions
            ("catenary --frobnicate", DenyCatenary), // unknown flag stays closed
            ("catenary --version extra", DenyCatenary), // sole-flag only (misc 142)
            ("catenary -h topic", DenyCatenary), // sole-flag only (misc 142)
        ];
        for (cmd, want) in cases {
            assert_eq!(&outcome(cmd, &rules), want, "outcome for {cmd:?}");
        }
    }

    // ── Loop constructs in subshell position (bug 139) ──────────────────────

    #[test]
    fn bug139_subshell_loops_end_to_end() {
        use DenialReason::OpaqueWrite;
        use Outcome::{Allow, DenyForeign};
        let rules = recommended_rules();
        let cases: &[(&str, Outcome)] = &[
            // The sighting: a subshell `for` of an allowed command (`echo`) must
            // be ALLOWED — the leading `(` no longer names the loop variable `f`
            // as the denied command.
            (r#"(for f in *; do echo "== $f"; done)"#, Allow),
            // The whole sighting line, `cd`-prefixed and `&&`-chained.
            (
                r#"cd src 2>/dev/null && (for f in *; do echo "== $f"; done)"#,
                Allow,
            ),
            // The blessed polling-wait idiom, subshelled: `cat`/`grep`/`sleep` are
            // all permitted, and `done)` is no longer read as a command.
            ("(until cat f | grep -q x; do sleep 5; done)", Allow),
            ("(while true; do sleep 1; done)", Allow),
            // The allowance shortcut only skips the iteration list when the body
            // is non-editing: a write whose target rides the loop variable engages
            // the hard path and fails toward the resolver's deny — never silently
            // allowed.
            (
                "(for f in *; do echo x > $f; done)",
                DenyForeign(OpaqueWrite),
            ),
            // A denied command in the body denies on *that* command, not the loop
            // variable (`cargo` is not allowlisted).
            (
                "(for f in *; do cargo build; done)",
                DenyForeign(DenialReason::NotAllowed),
            ),
        ];
        for (cmd, want) in cases {
            assert_eq!(&outcome(cmd, &rules), want, "outcome for {cmd:?}");
        }
    }

    #[test]
    fn bug139_write_to_loop_var_names_the_tainted_variable() {
        // The hard-path denial carries the resolver's construct-naming teaching
        // message — it names the per-iteration variable, not `f` as a command.
        let rules = recommended_rules();
        let denial = check_command("(for f in *; do echo x > $f; done)", &rules, None)
            .expect("a write riding the loop variable must deny");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
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
    // half. The faithful parse (ticket 08) makes this fall out structurally: a
    // newline is one of the list operators that separate pipelines, and the
    // parse strips heredoc bodies and their closing-delimiter lines and never
    // splits a newline inside a quoted arg or a `(…)`/backtick grouping — so the
    // multi-line `git commit` heredoc form is never split. These assertions were
    // the `known_gap_newline_not_a_separator` pins (CatenaryInternal bugs/20);
    // they are now flipped to the closed behavior.

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

    // ── Compound + isolation routing on the faithful parse (ticket 04) ─
    //
    // The catenary-regime classification and the isolation gate run on the
    // `ParsedScript`. The isolation gate is *structural* — it counts the parse's
    // command positions and reads its compound flag — so an under-counted
    // separator can never mistake a chained correlated command for an isolated
    // one (the daemon-wedge hazard, decision 020 §5). Compounds of allowlisted
    // commands fall through to the per-command allowlist walk (the `for`-loop
    // flip). The ticket's named cases:

    #[test]
    fn ticket04_bare_diagnostics_is_isolated() {
        // `catenary diagnostics` → isolated, handed off (allowed).
        assert_eq!(
            analyze_catenary_command("catenary diagnostics", None),
            CatenaryAction::Diagnostics,
        );
        // Scoped paths keep it one `SimpleCommand`, so the count-based gate
        // still passes it as isolated — the paths are a first-class scoped set
        // (ws37 ticket 02), not a denial trigger.
        assert_eq!(
            analyze_catenary_command("catenary diagnostics src/main.rs", None),
            CatenaryAction::Diagnostics,
        );
    }

    #[test]
    fn ws37_scoped_diagnostics_paths_are_not_a_denial_trigger() {
        // ws37 ticket 02: `catenary diagnostics <paths>` as the sole command is
        // a valid scoped pull — positional args never turn it into a denial, and
        // the bare-only isolation gate is untouched (a *chained* scoped form
        // still denies on isolation, below).
        for cmd in [
            "catenary diagnostics src/main.rs",
            "catenary diagnostics src/main.rs src/lib.rs",
            "catenary diagnostics ./relative/path.rs",
            "catenary diagnostics .",
        ] {
            assert_eq!(
                analyze_catenary_command(cmd, None),
                CatenaryAction::Diagnostics,
                "scoped sole-command diagnostics must stay handed-off, not denied: {cmd}",
            );
        }
        // The isolation gate itself is NOT weakened: a scoped pull chained after
        // another command still denies (the daemon-wedge hazard is structural,
        // counting command positions — not a per-arg check).
        assert!(
            matches!(
                analyze_catenary_command("sleep 1; catenary diagnostics src/main.rs", None),
                CatenaryAction::Deny(_),
            ),
            "a chained scoped diagnostics must still deny on isolation",
        );
    }

    #[test]
    fn ticket04_sleep_then_diagnostics_denied_daemon_wedge() {
        // `sleep 100; catenary diagnostics` → denied (isolation). A non-isolated
        // correlated command would wedge the daemon — caught by the command
        // *count* (two `SimpleCommand`s), never a substring scan.
        assert!(
            matches!(
                analyze_catenary_command("sleep 100; catenary diagnostics", None),
                CatenaryAction::Deny(_),
            ),
            "a chained `catenary diagnostics` must deny on isolation",
        );
        assert!(deny_text("sleep 100; catenary diagnostics").contains("its own"));
    }

    #[test]
    fn ticket04_for_loop_diagnostics_denied_isolation() {
        // `for f in *.rs; do catenary diagnostics; done` → denied (isolation):
        // the catenary command is in a compound, so it is not the sole command.
        assert!(
            matches!(
                analyze_catenary_command("for f in *.rs; do catenary diagnostics; done", None),
                CatenaryAction::Deny(_),
            ),
            "a `for`-loop body `catenary diagnostics` must deny on isolation",
        );
        assert!(deny_text("for f in *.rs; do catenary diagnostics; done").contains("its own"),);
    }

    #[test]
    fn ticket04_cd_then_grep_allowed() {
        // `cd src && catenary grep foo` → allowed (grep chains freely).
        assert_eq!(
            analyze_catenary_command("cd src && catenary grep foo", None),
            CatenaryAction::Allow { has_foreign: true },
        );
    }

    #[test]
    fn ticket06_retired_catenary_sed_redirects() {
        // `catenary sed` is retired (ws38 06): any form — bare, chained, or
        // `--in-place` — gets the teaching redirect to native `sed -i`.
        for cmd in [
            "catenary sed -e 's/a/b/' f.rs",
            "cd src && catenary sed -e 's/a/b/' f.rs",
            "catenary sed --in-place a b f.rs; echo done",
        ] {
            assert!(
                matches!(
                    analyze_catenary_command(cmd, None),
                    CatenaryAction::Deny(ref m) if m.contains("`catenary sed` is retired"),
                ),
                "`{cmd}` should teach native sed -i",
            );
        }
    }

    #[test]
    fn ticket04_for_loop_of_allowlisted_git_allowed() {
        // `for f in *.rs; do git add "$f"; done` → allowed (compound of
        // allowlisted `git`). The loop variable `f` is structure, not a command.
        let rules = recommended_rules();
        assert!(
            check_command(r#"for f in *.rs; do git add "$f"; done"#, &rules, None).is_none(),
            "a `for`-loop of an allowlisted command must be allowed",
        );
        assert_eq!(
            outcome(r#"for f in *.rs; do git add "$f"; done"#, &rules),
            Outcome::Allow,
        );
    }

    #[test]
    fn ticket04_for_loop_of_denied_cargo_denied() {
        // `for f in *; do cargo build; done` → denied on `cargo` (allowlist,
        // inside the compound).
        let rules = recommended_rules();
        let denial = check_command("for f in *; do cargo build; done", &rules, None)
            .expect("cargo inside a for loop must be denied");
        assert_eq!(denial.command, "cargo");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn ticket04_for_loop_redirect_style_sed_denied() {
        // `for f in *; do sed -i s/a/b/ "$f"; done` → denied: `sed` is
        // allowlisted (ws38 06), but the per-iteration loop variable `$f` is a
        // computed target the resolver can't see, so the write is opaque.
        let rules = recommended_rules();
        let denial = check_command(r#"for f in *; do sed -i s/a/b/ "$f"; done"#, &rules, None)
            .expect("sed with an opaque loop-variable target must be denied");
        assert_eq!(denial.command, "sed");
        assert_eq!(denial.reason, DenialReason::OpaqueWrite);
    }

    #[test]
    fn bug137_case_scrutinee_not_named_as_command() {
        // The ticket repro: a `case` over an allowlisted arm-body command surface
        // must be allowed, and the scrutinee variable `$c` must never be resolved
        // into command position (the misparse it fixes).
        let rules = recommended_rules();
        let repro = r#"c=hello; case "$c" in hello) true;; *) false;; esac"#;
        assert!(
            check_command(repro, &rules, None).is_none(),
            "a `case` of allowlisted arm bodies must be allowed (bug 137)",
        );
        assert_eq!(outcome(repro, &rules), Outcome::Allow);
    }

    #[test]
    fn bug137_case_denied_arm_names_the_offending_command() {
        // A denied command in an arm body denies with *that* command named — not
        // the scrutinee, not `case`.
        let rules = recommended_rules();
        let denial = check_command("case $x in a) cargo build;; esac", &rules, None)
            .expect("cargo inside a case arm must be denied");
        assert_eq!(denial.command, "cargo");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn bug137_case_nested_denied_arm_names_offending_command() {
        // A denied command in a *nested* case arm is still surfaced and named.
        let rules = recommended_rules();
        let denial = check_command(
            "case $x in a) case $y in b) cargo test;; esac;; esac",
            &rules,
            None,
        )
        .expect("cargo inside a nested case arm must be denied");
        assert_eq!(denial.command, "cargo");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn bug137_case_fallthrough_terminator_arm_denied() {
        // The fallthrough terminators (`;&`, `;;&`) still delimit arms whose
        // bodies filter — a denied command after one is caught.
        let rules = recommended_rules();
        let denial = check_command("case $x in a) echo a;& b) cargo build;; esac", &rules, None)
            .expect("cargo after a `;&` fallthrough arm must be denied");
        assert_eq!(denial.command, "cargo");
    }

    #[test]
    fn bug137_case_glued_body_denied_arm_names_offending_command() {
        // Bug 137 review: a body command glued to the pattern's `)` (no space)
        // must still deny naming it — the earlier fix dropped the glued tail,
        // failing *open* on a denied command the shell would run on a match.
        let rules = recommended_rules();
        let denial = check_command("case $x in a)cargo build;; esac", &rules, None)
            .expect("glued `cargo` inside a case arm must be denied");
        assert_eq!(denial.command, "cargo");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn bug137_case_glued_body_all_allowed_still_allows() {
        // The glued-close ticket-shaped repro of *allowed* arm bodies stays
        // allowed — the split surfaces the bodies without inventing a denial.
        let rules = recommended_rules();
        let repro = r#"case "$c" in hello)true;; *)false;; esac"#;
        assert!(
            check_command(repro, &rules, None).is_none(),
            "a glued `case` of allowlisted arm bodies must be allowed (bug 137)",
        );
        assert_eq!(outcome(repro, &rules), Outcome::Allow);
    }

    #[test]
    fn bug137_case_glued_body_with_alternation_denied() {
        // A glued body after an alternation pattern (`a|b)cargo`) denies naming
        // the offending body command — the `|` is pattern structure.
        let rules = recommended_rules();
        let denial = check_command("case $x in a|b)cargo build;; esac", &rules, None)
            .expect("glued `cargo` after an alternation pattern must be denied");
        assert_eq!(denial.command, "cargo");
        assert_eq!(denial.reason, DenialReason::NotAllowed);
    }

    #[test]
    fn ticket04_isolation_gate_is_structural_not_substring() {
        // The hazard guard: a correlated command quoted inside an argument is
        // *not* a command position, so it never trips the isolation gate (it is
        // not even recognized); and a real chained one is caught by the command
        // count, not by scanning for the word "diagnostics". Both directions
        // confirm the gate reads the parse's structure, never raw text.
        assert_eq!(
            analyze_catenary_command(
                r#"git commit -m "ran catenary diagnostics on the tree""#,
                None
            ),
            CatenaryAction::NotCatenary,
            "a quoted `catenary diagnostics` is prose, not a command",
        );
        // A genuine second command is seen structurally (count == 2 → deny).
        assert!(matches!(
            analyze_catenary_command("true && catenary diagnostics", None),
            CatenaryAction::Deny(_),
        ));
    }

    #[test]
    fn ticket08_catenary_words_in_heredoc_body_do_not_trip_gates() {
        // Required ticket-08 outcome: a `catenary diagnostics` named in a commit
        // heredoc *body* is opaque stdin — the parse strips the body before
        // either gate runs, so neither the isolation/catenary gate nor the
        // foreign allowlist ever sees it.
        let cmd = "git commit -F - <<EOF\n\
                   refactor: ran catenary diagnostics; everything is green\n\
                   also tidied catenary sed --in-place call sites\n\
                   EOF";
        // The catenary gate finds no `catenary` command — the body is gone, so
        // the only command is the foreign `git`.
        assert_eq!(
            analyze_catenary_command(cmd, None),
            CatenaryAction::NotCatenary,
            "a `catenary diagnostics` in a heredoc body must not be recognized",
        );
        // The foreign allowlist sees only `git` (allowed) — the body prose,
        // including its `;`, never segments or reaches the gate.
        let rules = recommended_rules();
        assert!(
            check_command(cmd, &rules, None).is_none(),
            "heredoc body prose must not deny the allowlisted `git`",
        );
    }

    #[test]
    fn ticket04_compound_table() {
        use Outcome::{Allow, DenyCatenary, DenyForeign};
        let rules = recommended_rules();
        let cases: &[(&str, Outcome)] = &[
            // Isolation: correlated commands must be the sole command.
            ("catenary diagnostics", Allow),
            ("sleep 100; catenary diagnostics", DenyCatenary),
            ("for f in *.rs; do catenary diagnostics; done", DenyCatenary),
            // `catenary sed` is retired → teaching redirect.
            ("catenary sed --in-place a b f.rs; echo done", DenyCatenary),
            ("cd src && catenary sed -e 's/a/b/' f.rs", DenyCatenary),
            // Chain-free: search carries no handoff.
            ("cd src && catenary grep foo", Allow),
            // Compound allow: a `for` loop of allowlisted commands runs.
            (r#"for f in *.rs; do git add "$f"; done"#, Allow),
            // Compound deny: the allowlist still gates every body command.
            (
                "for f in *; do cargo build; done",
                DenyForeign(DenialReason::NotAllowed),
            ),
            // Native `sed` is allowlisted, but the loop variable `$f` is an
            // opaque target — the resolver denies the write.
            (
                r#"for f in *; do sed -i s/a/b/ "$f"; done"#,
                DenyForeign(DenialReason::OpaqueWrite),
            ),
        ];
        for (cmd, want) in cases {
            assert_eq!(&outcome(cmd, &rules), want, "outcome for {cmd:?}");
        }
    }

    // ── handoff-command pipe-out deny message coverage ──────────────

    #[test]
    fn handoff_pipe_out_teaches_bare_form_no_stale_hints() {
        // Piping a handoff-carrying command (`diagnostics` here) is a bare-only
        // violation. The message teaches the bare form and names the downstream,
        // and carries none of the retired volume hints (spill file / runtime-dir
        // report / `--page` / overflow valve).
        for down in ["head", "tail", "wc"] {
            let msg = deny_text(&format!("catenary diagnostics | {down}"));
            assert!(
                msg.contains("bare"),
                "must teach the bare form, got: {msg:?}"
            );
            assert!(
                msg.contains(down),
                "should name the downstream `{down}`, got: {msg:?}",
            );
            for stale in ["--page", "runtime-dir", "overflow valve", "spill"] {
                assert!(
                    !msg.contains(stale),
                    "stale hint `{stale}` must be gone, got: {msg:?}",
                );
            }
        }
    }

    // ── set_cli_command / render_subcommand_help coverage ───────────
    //
    // `CLI_COMMAND` is a process-global `OnceLock`, so a single test owns both
    // the write (set_cli_command) and the reads (render_subcommand_help). No
    // other lib test touches the static, so this is the sole writer.

    #[test]
    fn cli_command_set_and_subcommand_help_rendered() {
        let cli = clap::Command::new("catenary")
            .subcommand(clap::Command::new("grep").about("Search code with structured results"));
        set_cli_command(cli);

        // set_cli_command must actually install the command (kills `-> ()`):
        // an unset OnceLock leaves render_subcommand_help returning empty.
        let help = render_subcommand_help("grep");
        assert!(
            help.contains("Search code with structured results"),
            "rendered help must carry the subcommand's about text, got: {help:?}",
        );
        assert!(
            help.contains("catenary grep"),
            "rendered help must use the `catenary grep` bin name, got: {help:?}",
        );
        // Kills `-> "xyzzy".into()`: real help never equals the constant.
        assert_ne!(help.trim(), "xyzzy");

        // An unknown subcommand returns empty — pins the not-found path and kills
        // `-> "xyzzy".into()` (which would return non-empty here).
        assert!(
            render_subcommand_help("no-such-subcommand").is_empty(),
            "an unknown subcommand must render empty",
        );
    }
}
