// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Write resolver: classify every segment of a shell command into
//! Recorded / PureDelete / NoWrite / Opaque (ws38 ticket 01, decision 026).
//!
//! **Resolve-or-deny.** For every command the filter tokenizes, the resolver
//! produces the **complete, exact** set of paths the command writes — from
//! shell grammar (redirects, heredoc targets), argument convention (`cp`,
//! `mv`, `tee`, `sed -i`, `rsync`), or a **state query** (hook-expanded globs,
//! filesystem dir-ness / tree enumeration) — or it classifies the segment
//! **Opaque** and the caller denies with a construct-naming teaching message.
//! Partial resolution is the one forbidden outcome (decision 019's "never
//! silently under-return", applied to write-sets): when a form is not
//! certainly covered, it is Opaque.
//!
//! **The fail direction is asymmetric.** Targets are recorded at
//! `PreToolUse`, before execution — a recorded path that never gets written
//! is phantom debt (safe); an actual write missing from the set is the
//! untracked write this design exists to kill (forbidden). That asymmetry
//! licenses generous resolution: hook-time glob expansion with globstar-on
//! superset semantics, dir-tree over-enumeration, and recording both
//! interpretations when a `cp` destination's dir-ness cannot be queried.
//!
//! **Query, never evaluate.** Resolution may read tracked state (the
//! filesystem for a glob or a dir-ness check) because the hook is blocking —
//! hook-time state *is* execution-time state. It may never evaluate the
//! command's dynamic content: command substitutions, inherited environment
//! variables, and stdin-driven target lists are Opaque by construction.
//!
//! **Sequential composition is union-safe.** Segments resolve in document
//! order against threaded state (variable bindings, the effective cwd); the
//! line's recorded set is the union of the per-segment sets. A pure delete
//! only shrinks later expansions; a segment that mutates queried state is
//! itself a recorded write form; an opaque `cd` poisons every downstream
//! relative target.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use super::parse::{
    self, Assignment, ListOp, ParsedScript, Redirect, RedirectOp, SimpleCommand, WordMeta,
};

/// Checkable interpreter programs — the `awk` / `perl` program-check arm
/// (ws38 ticket 04, decision 026, soundness layer 4).
mod programs;

/// Cap on filesystem entries visited by one glob expansion / tree walk. A
/// pattern that would exceed it is classified Opaque ("too broad") rather
/// than stalling the hook.
const MAX_WALK_ENTRIES: usize = 25_000;

/// Cap on the words one brace expansion may produce.
const MAX_BRACE_WORDS: usize = 64;

// ── Public result types ──────────────────────────────────────────────────────

/// An unresolvable write: the construct that defeated resolution plus the
/// teaching denial message. Complete-or-deny's deny arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpaqueWrite {
    /// The command word of the segment that failed to resolve.
    pub command: String,
    /// Short machine-readable name of the unresolvable construct.
    pub construct: &'static str,
    /// The teaching message: names the construct and the resolvable form.
    pub message: String,
}

/// How one command segment touches the working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SegmentClass {
    /// The complete write-set resolved — paths to attribute (may be empty
    /// when e.g. every target is a device sink).
    Recorded(BTreeSet<PathBuf>),
    /// Deletes only — no diagnostics debt (nothing to diagnose in a gone
    /// file); coherence owns deletions, as it always has.
    PureDelete,
    /// No writes.
    NoWrite,
    /// A write occurs whose targets cannot be resolved — deny.
    Opaque(OpaqueWrite),
}

/// The resolved write-set of a whole command line: the union of every
/// segment's recorded set (ticket 02 attributes these into the issuing
/// session's tracked set).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineWrites {
    /// Every path the command line can write, resolved at hook time.
    pub writes: BTreeSet<PathBuf>,
}

/// Resolve a full shell command line: parse it and resolve every segment.
///
/// `cwd` is the host-reported working directory of the tool call; relative
/// targets resolve against it. With `cwd = None`, literal relative targets
/// are recorded as relative paths, and resolutions that need a filesystem
/// query (globs, dir-ness, tree enumeration) classify Opaque.
///
/// # Errors
///
/// Returns the first [`OpaqueWrite`] (document order) when any segment
/// writes through a form the resolver cannot see completely.
pub fn resolve_command(cmd: &str, cwd: Option<&Path>) -> Result<LineWrites, OpaqueWrite> {
    resolve_script(&parse::parse(cmd), cwd)
}

/// Resolve an already-parsed script (the same parse the allowlist walk
/// reads). See [`resolve_command`].
///
/// # Errors
///
/// Returns the first [`OpaqueWrite`] in document order.
pub(crate) fn resolve_script(
    script: &ParsedScript,
    cwd: Option<&Path>,
) -> Result<LineWrites, OpaqueWrite> {
    let mut state = State::new(cwd);
    let mut writes = BTreeSet::new();
    resolve_into(script, &mut state, &mut writes)?;
    Ok(LineWrites { writes })
}

// ── Threaded state ───────────────────────────────────────────────────────────

/// The effective working directory threaded across segments.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cwd {
    /// Known absolute directory.
    Abs(PathBuf),
    /// Unknown host cwd plus a literal relative prefix accumulated by `cd`
    /// (empty when no `cd` has run). Relative targets resolve to relative
    /// paths under this prefix; filesystem queries are impossible.
    Rel(PathBuf),
    /// An opaque `cd` ran — every downstream relative target is Opaque.
    Poisoned,
}

/// A variable binding observed in the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    /// Bound unconditionally to a plain literal value.
    Literal(String),
    /// Bound to something unresolvable (conditional, computed, subshell, or
    /// loop-scoped) — any use in a write target is Opaque.
    Tainted,
}

/// Resolver state threaded across a script's segments in document order.
#[derive(Debug, Clone)]
struct State {
    cwd: Cwd,
    bindings: HashMap<String, Binding>,
    /// A variable-mutating command (`read`, `declare`, `export`, …) ran:
    /// every later `$…` write target is unresolvable.
    vars_tainted: bool,
}

impl State {
    fn new(cwd: Option<&Path>) -> Self {
        Self {
            cwd: cwd.map_or_else(|| Cwd::Rel(PathBuf::new()), |p| Cwd::Abs(p.to_path_buf())),
            bindings: HashMap::new(),
            vars_tainted: false,
        }
    }
}

/// Per-segment context from the list structure.
#[derive(Debug, Clone, Copy)]
struct SegCtx {
    /// The segment executes conditionally (its pipeline follows `&&`/`||`),
    /// so an assignment in it cannot be trusted as a binding.
    conditional: bool,
    /// The segment is the only stage of its pipeline. Multi-stage segments
    /// run in subshells: their assignments and `cd`s never escape.
    sole_stage: bool,
}

/// Walk a script's pipelines in document order, resolving every segment and
/// unioning recorded writes into `writes`.
fn resolve_into(
    script: &ParsedScript,
    state: &mut State,
    writes: &mut BTreeSet<PathBuf>,
) -> Result<(), OpaqueWrite> {
    let mut conditional = false;
    for pipeline in &script.pipelines {
        let sole_stage = pipeline.commands.len() == 1;
        for command in &pipeline.commands {
            let ctx = SegCtx {
                conditional,
                sole_stage,
            };
            match resolve_segment(command, state, ctx) {
                SegmentClass::Recorded(paths) => writes.extend(paths),
                SegmentClass::Opaque(op) => return Err(op),
                SegmentClass::PureDelete | SegmentClass::NoWrite => {}
            }
        }
        conditional = matches!(pipeline.terminator, Some(ListOp::And | ListOp::Or));
    }
    Ok(())
}

// ── Segment resolution ───────────────────────────────────────────────────────

/// An unresolvable construct, before it is attributed to a segment's command
/// word (which turns it into an [`OpaqueWrite`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unresolved {
    construct: &'static str,
    message: String,
}

impl Unresolved {
    fn into_opaque(self, command: &str) -> OpaqueWrite {
        OpaqueWrite {
            command: command.to_string(),
            construct: self.construct,
            message: self.message,
        }
    }
}

/// Shorthand constructor for [`Unresolved`].
fn u(construct: &'static str, message: impl Into<String>) -> Unresolved {
    Unresolved {
        construct,
        message: message.into(),
    }
}

/// Resolve one segment: substitutions (subshells), redirects (uniform shell
/// grammar), variable effects, then the per-command registry.
fn resolve_segment(cmd: &SimpleCommand, state: &mut State, ctx: SegCtx) -> SegmentClass {
    let seg_name = cmd.name.clone().unwrap_or_else(|| "shell".to_string());

    // Command substitutions run in subshells: resolve each against a *clone*
    // of the state (their `cd`s / bindings never escape), but their writes
    // are real and union into this segment's recorded set.
    let mut recorded: BTreeSet<PathBuf> = BTreeSet::new();
    for sub in &cmd.substitutions {
        let mut sub_state = state.clone();
        let mut sub_writes = BTreeSet::new();
        if let Err(op) = resolve_into(sub, &mut sub_state, &mut sub_writes) {
            return SegmentClass::Opaque(op);
        }
        recorded.extend(sub_writes);
    }

    // Redirects: shell grammar, uniform over every command (soundness layer 1).
    match resolve_redirects(cmd, state) {
        Ok(paths) => recorded.extend(paths),
        Err(unres) => return SegmentClass::Opaque(unres.into_opaque(&seg_name)),
    }

    // A `for`/`select` loop variable is per-iteration — never resolvable.
    if let Some(var) = &cmd.loop_var {
        state.bindings.insert(var.clone(), Binding::Tainted);
    }
    // Assignments swept out of a compound (`( F=x; … )`, `{ F=x; … }`,
    // arithmetic) cannot be scoped soundly — taint them.
    if cmd.is_compound {
        for a in &cmd.assignments {
            state.bindings.insert(a.name.clone(), Binding::Tainted);
        }
    }

    // Bare-assignment segment (no command word): bind or taint.
    let Some(name) = cmd.name.as_deref() else {
        if !cmd.is_compound && ctx.sole_stage {
            for a in &cmd.assignments {
                bind_assignment(a, state, ctx.conditional);
            }
        }
        // Multi-stage assignments run in pipeline subshells — they never
        // escape, so they neither bind nor taint.
        return finish(recorded, SegmentClass::NoWrite);
    };

    // Variable mutators rebind at runtime: later `$…` targets are opaque.
    if is_var_mutator(name, &cmd.argv) {
        state.vars_tainted = true;
        return finish(recorded, SegmentClass::NoWrite);
    }

    if name == "cd" {
        apply_cd(cmd, state, ctx);
        return finish(recorded, SegmentClass::NoWrite);
    }

    // The registry: argument-convention writers, wrappers, and the explicit
    // opaque executors. Everything else is NoWrite — program-internal writes
    // of allowlisted tools keep decision 022's inherited accepted boundary
    // (soundness layer 4), and git keeps its current allowlist treatment
    // untouched until ticket 03.
    let class = match name {
        "rm" | "rmdir" => SegmentClass::PureDelete,
        "cp" => run(resolve_cp(cmd, state), &seg_name),
        "mv" => run(resolve_mv(cmd, state), &seg_name),
        "tee" => run(resolve_tee(cmd, state), &seg_name),
        "sed" => run(resolve_sed(cmd, state), &seg_name),
        "rsync" => run(resolve_rsync(cmd, state), &seg_name),
        // Checkable interpreter programs (ws38 ticket 04): a literal awk/perl
        // program is data iff it parses into the pure filter/substitution
        // subset — parsed, not trusted.
        "awk" | "gawk" | "mawk" | "nawk" => run(programs::resolve_awk(cmd, state), &seg_name),
        "perl" => run(programs::resolve_perl(cmd, state), &seg_name),
        "bash" | "sh" | "zsh" | "dash" | "ksh" => resolve_shell_wrapper(cmd, state, &seg_name),
        "xargs" => resolve_xargs(cmd, state, &seg_name),
        "dd" | "install" | "truncate" => SegmentClass::Opaque(
            u(
                "unmodeled-writer",
                format!(
                    "`{name}` writes files in a way the resolver doesn't model yet, so its \
                     write-set can't be attributed. Use `cp`/`mv` or a redirect, whose \
                     targets resolve."
                ),
            )
            .into_opaque(&seg_name),
        ),
        "eval" | "source" | "." => SegmentClass::Opaque(
            u(
                "dynamic-execution",
                format!(
                    "`{name}` executes dynamically assembled commands the hook can't \
                     resolve, so any write inside it would go unattributed. Run the \
                     commands directly."
                ),
            )
            .into_opaque(&seg_name),
        ),
        _ => SegmentClass::NoWrite,
    };
    finish(recorded, class)
}

/// Convert a per-command resolution into a [`SegmentClass`], attributing an
/// unresolved construct to the segment's command word.
fn run(result: Result<SegmentClass, Unresolved>, seg_name: &str) -> SegmentClass {
    result.unwrap_or_else(|unres| SegmentClass::Opaque(unres.into_opaque(seg_name)))
}

/// Merge the substitution/redirect writes gathered so far with the
/// command-level class. Opaque dominates; any recorded path promotes the
/// segment to Recorded.
fn finish(recorded: BTreeSet<PathBuf>, class: SegmentClass) -> SegmentClass {
    match class {
        SegmentClass::Opaque(op) => SegmentClass::Opaque(op),
        SegmentClass::Recorded(more) => {
            let mut all = recorded;
            all.extend(more);
            SegmentClass::Recorded(all)
        }
        SegmentClass::PureDelete => {
            if recorded.is_empty() {
                SegmentClass::PureDelete
            } else {
                SegmentClass::Recorded(recorded)
            }
        }
        SegmentClass::NoWrite => {
            if recorded.is_empty() {
                SegmentClass::NoWrite
            } else {
                SegmentClass::Recorded(recorded)
            }
        }
    }
}

// ── Variable effects ─────────────────────────────────────────────────────────

/// Whether `name` can rebind shell variables at runtime, defeating every
/// later `$…` resolution.
fn is_var_mutator(name: &str, argv: &[String]) -> bool {
    match name {
        "read" | "readarray" | "mapfile" | "declare" | "typeset" | "local" | "export" | "unset"
        | "readonly" | "let" => true,
        "printf" => argv.iter().any(|a| a == "-v"),
        _ => false,
    }
}

/// Bind a bare-assignment segment's variable: a plain literal value binds;
/// anything conditional or computed taints.
fn bind_assignment(a: &Assignment, state: &mut State, conditional: bool) {
    let binding = if conditional {
        Binding::Tainted
    } else {
        resolve_binding_value(a, state)
    };
    state.bindings.insert(a.name.clone(), binding);
}

/// Resolve an assignment's value to a plain literal, or taint. A binding is
/// usable in a write target only when its value expands identically whether
/// the use site quotes it or not — i.e. it contains no globs, braces,
/// whitespace, or further expansions.
fn resolve_binding_value(a: &Assignment, state: &State) -> Binding {
    let m = a.meta;
    if m.value_subs || m.process_subs || m.live_glob || m.live_brace {
        return Binding::Tainted;
    }
    if m.literal_meta && m.live_dollar {
        return Binding::Tainted;
    }
    let mut value = a.value.clone();
    if m.live_dollar {
        match substitute_vars(&value, state) {
            Ok(v) => value = v,
            Err(_) => return Binding::Tainted,
        }
    }
    // Assignment-value tilde (`F=~/x`) expands when unquoted.
    if (value == "~" || value.starts_with("~/")) && !m.literal_meta {
        let Some(home) = dirs::home_dir() else {
            return Binding::Tainted;
        };
        value = if value == "~" {
            home.to_string_lossy().into_owned()
        } else {
            home.join(&value[2..]).to_string_lossy().into_owned()
        };
    }
    if is_plain_value(&value) {
        Binding::Literal(value)
    } else {
        Binding::Tainted
    }
}

/// Whether a bound value is plain: expands to itself at any use site
/// (no globs, braces, whitespace, or `$`/backtick).
fn is_plain_value(value: &str) -> bool {
    !value.chars().any(|c| {
        c.is_whitespace() || c.is_control() || matches!(c, '$' | '`' | '*' | '?' | '[' | '{' | '}')
    })
}

/// Apply a `cd` segment to the threaded cwd. Failure to resolve never denies
/// by itself — it poisons the cwd, and only a later *relative* write target
/// turns that into an opaque denial.
fn apply_cd(cmd: &SimpleCommand, state: &mut State, ctx: SegCtx) {
    if !ctx.sole_stage {
        // A pipeline-stage `cd` runs in a subshell and never escapes.
        return;
    }
    if cmd.is_compound {
        // `( cd … )` / loop-body `cd`: subshell scoping the flat sweep can't
        // model — fail toward poison.
        state.cwd = Cwd::Poisoned;
        return;
    }
    let Some(target) = cmd.argv.first() else {
        state.cwd = dirs::home_dir().map_or(Cwd::Poisoned, Cwd::Abs);
        return;
    };
    if target == "-" {
        state.cwd = Cwd::Poisoned;
        return;
    }
    let meta = cmd.argv_meta.first().copied().unwrap_or_default();
    match expand_word(target, meta, state, Position::Single) {
        Ok(paths) if paths.len() == 1 => {
            let p = paths.into_iter().next().unwrap_or_default();
            state.cwd = if p.is_absolute() {
                Cwd::Abs(p)
            } else {
                Cwd::Rel(p)
            };
        }
        _ => state.cwd = Cwd::Poisoned,
    }
}

// ── Redirect resolution (soundness layer 1) ──────────────────────────────────

/// Resolve every output redirect on a segment to its target paths. Sinks
/// (`/dev/null`-family, `2>&1`, `>&2`, `>&-`) are not writes; `> >(cmd)` is a
/// pipe whose inner command is resolved through the substitution recursion.
fn resolve_redirects(cmd: &SimpleCommand, state: &State) -> Result<BTreeSet<PathBuf>, Unresolved> {
    let mut out = BTreeSet::new();
    for redirect in &cmd.redirects {
        match redirect.op {
            RedirectOp::Read | RedirectOp::DupIn | RedirectOp::HereString => {}
            RedirectOp::DupOut => {
                if super::is_fd_dup_target(&redirect.target)
                    || super::target_is_device_sink(&redirect.target)
                {
                    continue;
                }
                out.extend(expand_redirect_target(redirect, state)?);
            }
            RedirectOp::Write | RedirectOp::Append | RedirectOp::WriteBoth => {
                if super::target_is_device_sink(&redirect.target) {
                    continue;
                }
                out.extend(expand_redirect_target(redirect, state)?);
            }
        }
    }
    Ok(out)
}

/// Expand one output redirect's target word. Multiple expansion results are
/// all recorded (zsh MULTIOS writes them all; bash errors and writes none —
/// over-recording is the safe direction).
fn expand_redirect_target(redirect: &Redirect, state: &State) -> Result<Vec<PathBuf>, Unresolved> {
    if redirect.target.is_empty()
        && !redirect.target_meta.process_subs
        && !redirect.target_meta.value_subs
    {
        return Err(u(
            "dangling-redirect",
            "A redirect operator with no target can't be resolved (the shell would \
             reject it). Name the target file.",
        ));
    }
    expand_word(
        &redirect.target,
        redirect.target_meta,
        state,
        Position::Single,
    )
}

// ── Word expansion (statically expandable forms + state-query globs) ─────────

/// Where a word sits, for glob semantics: a single-file position (redirect
/// target, `cp`/`mv` destination) rejects globs; a file-list position
/// (`sed -i` files, `tee` operands, `cp`/`rsync` sources) expands them
/// against the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Position {
    Single,
    List,
}

/// Expand one word into the concrete path(s) it names, or classify why it
/// can't be expanded. The statically expandable forms — `${var}` bound in the
/// same command line, tilde, brace expansion — resolve here; globs in
/// file-list positions are expanded against the filesystem (a state query);
/// everything else is Opaque.
fn expand_word(
    text: &str,
    meta: WordMeta,
    state: &State,
    pos: Position,
) -> Result<Vec<PathBuf>, Unresolved> {
    if meta.value_subs {
        return Err(u(
            "command-substitution-target",
            "`$(…)`/backtick output names this write target at runtime, so the write \
             can't be attributed. Materialize the name first, then pass the path \
             literally.",
        ));
    }
    if meta.process_subs {
        if text.is_empty() {
            // A pure `>(cmd)` / `<(cmd)` word: a pipe, not a file — the inner
            // command's own writes are resolved through the substitution
            // recursion.
            return Ok(Vec::new());
        }
        return Err(u(
            "process-substitution-target",
            "A write target mixing text with `<(…)`/`>(…)` can't be resolved. Name a \
             plain file path.",
        ));
    }
    if text.is_empty() {
        return Err(u(
            "empty-target",
            "An empty write target can't be resolved. Name the target file.",
        ));
    }
    if meta.literal_meta && meta.any_live() {
        return Err(u(
            "mixed-quoting",
            "This write target mixes quoted and unquoted expansion characters, so the \
             hook can't tell which parts expand. Use a fully literal path, or a single \
             unquoted `$NAME` bound in this command.",
        ));
    }
    if !meta.any_live() {
        // Fully literal (metacharacters, if any, arrived through quotes).
        return Ok(filter_sinks(vec![resolve_path(text, state)?]));
    }

    // Live path: every metacharacter in the text is unquoted (literal_meta is
    // false), so each expansion family applies to the whole text.
    let mut words: Vec<String> = vec![text.to_string()];
    if meta.live_dollar {
        let mut substituted = Vec::new();
        for w in words {
            let s = substitute_vars(&w, state)?;
            if s.starts_with('-') && !w.starts_with('-') {
                return Err(u(
                    "computed-flag",
                    "A variable here expands to a `-` word, which the tool would read \
                     as a flag — that changes what gets written. Pass flags literally.",
                ));
            }
            substituted.push(s);
        }
        words = substituted;
    }
    if meta.live_brace {
        let mut expanded = Vec::new();
        for w in &words {
            expanded.extend(brace_expand(w)?);
        }
        words = expanded;
    }
    // Tilde: in the live path any `~` is unquoted, and brace expansion may
    // have exposed word-initial tildes (`~/{a,b}`).
    let mut tilded = Vec::new();
    for w in words {
        tilded.push(tilde_expand(&w)?);
    }
    let words = tilded;

    let mut paths = Vec::new();
    for w in &words {
        if w.is_empty() {
            return Err(u(
                "empty-target",
                "A write target here expands to an empty word. Name the target file.",
            ));
        }
        if has_glob_chars(w) {
            if pos == Position::Single {
                return Err(u(
                    "glob-single-target",
                    "A glob in a single-file write position is ambiguous (it can match \
                     several files or none). Name the file explicitly; globs resolve in \
                     file-list positions like `sed -i 's/a/b/' src/*.rs`.",
                ));
            }
            paths.extend(expand_glob(w, state)?);
        } else {
            paths.push(resolve_path(w, state)?);
        }
    }
    Ok(filter_sinks(paths))
}

/// Drop device sinks from a resolved path list — they never write the tree.
fn filter_sinks(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|p| p.to_str().is_none_or(|s| !super::target_is_device_sink(s)))
        .collect()
}

/// Whether a (post-substitution) word still carries glob metacharacters.
fn has_glob_chars(word: &str) -> bool {
    word.chars().any(|c| matches!(c, '*' | '?' | '['))
}

/// Resolve a non-glob word to a normalized path against the threaded cwd.
fn resolve_path(word: &str, state: &State) -> Result<PathBuf, Unresolved> {
    let p = Path::new(word);
    if p.is_absolute() {
        return Ok(normalize(p));
    }
    match &state.cwd {
        Cwd::Abs(base) => Ok(normalize(&base.join(p))),
        Cwd::Rel(prefix) => Ok(normalize(&prefix.join(p))),
        Cwd::Poisoned => Err(poisoned_cwd()),
    }
}

/// The opaque-cwd classification (an unresolvable `cd` upstream).
fn poisoned_cwd() -> Unresolved {
    u(
        "opaque-cwd",
        "An earlier `cd` in this command has an unresolvable target, so relative \
         write targets after it can't be resolved. `cd` to a literal directory, or \
         use absolute paths.",
    )
}

/// Normalize `.`/`..` lexically without touching the filesystem, preserving
/// leading `..` components of relative paths.
fn normalize(path: &Path) -> PathBuf {
    let mut parts: Vec<Component> = Vec::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(Component::Normal(_)) => {
                    parts.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => parts.push(comp),
            },
            other => parts.push(other),
        }
    }
    parts.iter().collect()
}

/// Substitute `$NAME` / `${NAME}` occurrences from the command line's own
/// bindings. Anything else under `$` — special/positional parameters,
/// `${…}` operator forms (defaults, indirection, arrays, slicing) — is
/// Opaque.
fn substitute_vars(text: &str, state: &State) -> Result<String, Unresolved> {
    if state.vars_tainted {
        return Err(u(
            "runtime-variables",
            "An earlier command here (`read`/`declare`/`export`/…) can rebind \
             variables at runtime, so `$…` write targets after it can't be resolved. \
             Write the target path literally.",
        ));
    }
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'$' {
                i += 1;
            }
            out.push_str(&text[start..i]);
            continue;
        }
        // A `$` at the end of the word is literal.
        if i + 1 >= bytes.len() {
            out.push('$');
            break;
        }
        match bytes[i + 1] {
            b'{' => {
                let close = text[i + 2..].find('}').map(|off| i + 2 + off);
                let Some(close) = close else {
                    return Err(param_form());
                };
                let name = &text[i + 2..close];
                if !is_var_name(name) {
                    return Err(param_form());
                }
                out.push_str(lookup_binding(name, state)?);
                i = close + 1;
            }
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                out.push_str(lookup_binding(&text[start..end], state)?);
                i = end;
            }
            _ => {
                return Err(u(
                    "special-parameter",
                    "Special/positional parameters (`$1`, `$@`, `$?`, …) in a write \
                     target can't be resolved at hook time. Pass a literal path.",
                ));
            }
        }
    }
    Ok(out)
}

/// The `${…}` operator-form classification.
fn param_form() -> Unresolved {
    u(
        "parameter-expansion-form",
        "`${…}` operator forms (defaults, indirection, arrays, slicing) can't be \
         statically resolved for a write target. Use a plain `$NAME` bound in this \
         command, or a literal path.",
    )
}

/// Whether `name` is a plain shell variable name.
fn is_var_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && (bytes[0].is_ascii_alphabetic() || bytes[0] == b'_')
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}

/// Look a variable up in the command line's own bindings.
fn lookup_binding<'a>(name: &str, state: &'a State) -> Result<&'a str, Unresolved> {
    match state.bindings.get(name) {
        Some(Binding::Literal(v)) => Ok(v),
        Some(Binding::Tainted) => Err(u(
            "tainted-variable",
            format!(
                "`${name}` is set conditionally, per-iteration, or from computed \
                 content in this command, so a write target using it can't be \
                 resolved. Bind it once, unconditionally (`{name}=out.txt; …`), or \
                 write the path literally."
            ),
        )),
        None => Err(u(
            "unbound-variable",
            format!(
                "`${name}` isn't bound in this command — the hook can't read the \
                 runtime environment, so the write can't be attributed. Bind it in \
                 the same command line (`{name}=out.txt; … > \"${name}\"`) or write \
                 the path literally."
            ),
        )),
    }
}

/// Expand tilde forms: `~` and `~/…` resolve to the home directory; `~user`
/// is Opaque. Words not starting with `~` pass through.
fn tilde_expand(word: &str) -> Result<String, Unresolved> {
    if !word.starts_with('~') {
        return Ok(word.to_string());
    }
    let home = || {
        dirs::home_dir().ok_or_else(|| {
            u(
                "tilde-no-home",
                "`~` can't be resolved (no home directory). Use an absolute path.",
            )
        })
    };
    if word == "~" {
        return Ok(home()?.to_string_lossy().into_owned());
    }
    if let Some(rest) = word.strip_prefix("~/") {
        return Ok(home()?.join(rest).to_string_lossy().into_owned());
    }
    Err(u(
        "tilde-user",
        "`~user` paths can't be resolved at hook time. Use an absolute path or `~/…`.",
    ))
}

/// Expand one brace group and recurse on the remainder: `{a,b}` comma lists
/// and `{N..M}` integer ranges. Groups bash would keep literal (`{a}`, a
/// lone brace) stay literal; nested or otherwise unrecognized groups are
/// Opaque (fail-closed).
fn brace_expand(word: &str) -> Result<Vec<String>, Unresolved> {
    let too_broad = || {
        u(
            "brace-expansion-form",
            "This brace expression isn't a form the hook can expand (`{a,b}` lists \
             and small `{N..M}` ranges are). Write the targets explicitly.",
        )
    };
    let Some(open) = word.find('{') else {
        return Ok(vec![word.to_string()]);
    };
    let rest = &word[open + 1..];
    let Some(close_off) = rest.find('}') else {
        // No closing brace: bash keeps the word literal.
        return Ok(vec![word.to_string()]);
    };
    let inner = &rest[..close_off];
    if inner.contains('{') {
        return Err(too_broad());
    }
    let prefix = &word[..open];
    let tail = &rest[close_off + 1..];

    let items: Vec<String> = if inner.contains(',') {
        inner.split(',').map(str::to_string).collect()
    } else if let Some(range) = parse_brace_range(inner) {
        range
    } else {
        // `{x}` without comma or range: bash keeps it literal. Recurse on the
        // tail in case a later group expands.
        return Ok(brace_expand(tail)?
            .into_iter()
            .map(|t| format!("{prefix}{{{inner}}}{t}"))
            .collect());
    };

    let tails = brace_expand(tail)?;
    let mut out = Vec::new();
    for item in &items {
        for t in &tails {
            out.push(format!("{prefix}{item}{t}"));
            if out.len() > MAX_BRACE_WORDS {
                return Err(too_broad());
            }
        }
    }
    Ok(out)
}

/// Parse a `{N..M}` integer range (ascending or descending, capped).
fn parse_brace_range(inner: &str) -> Option<Vec<String>> {
    let (a, b) = inner.split_once("..")?;
    let a: i64 = a.parse().ok()?;
    let b: i64 = b.parse().ok()?;
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let span = usize::try_from(hi.checked_sub(lo)?).ok()?;
    if span >= MAX_BRACE_WORDS {
        return None;
    }
    let mut items: Vec<String> = (lo..=hi).map(|n| n.to_string()).collect();
    if a > b {
        items.reverse();
    }
    Some(items)
}

// ── Glob expansion & filesystem queries (soundness layer 3) ──────────────────

/// Expand a glob pattern against the filesystem — a state query, sound
/// because the hook is blocking. Generous semantics (globstar on, dotfiles
/// included): the expansion may be a superset of the client shell's, which is
/// over-recording, the safe direction. Zero matches return the literal
/// pattern path (nullglob off: the shell passes the word through, and a
/// creator like `tee` writes a file by that literal name).
fn expand_glob(word: &str, state: &State) -> Result<Vec<PathBuf>, Unresolved> {
    let abs = match (Path::new(word).is_absolute(), &state.cwd) {
        (true, _) => normalize(Path::new(word)),
        (false, Cwd::Abs(base)) => normalize(&base.join(word)),
        (false, Cwd::Rel(_)) => {
            return Err(u(
                "no-cwd-for-query",
                "This glob needs a filesystem query, but the hook has no working \
                 directory for the call. Use absolute paths.",
            ));
        }
        (false, Cwd::Poisoned) => return Err(poisoned_cwd()),
    };
    let pattern = abs.to_string_lossy().into_owned();
    let matcher = globset::GlobBuilder::new(&pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map_err(|_| {
            u(
                "glob-form",
                "This pattern isn't a glob form the hook can expand (unclosed `[`?). \
                 Name the files explicitly.",
            )
        })?
        .compile_matcher();

    // Walk from the static prefix (components before the first glob char).
    let mut root = PathBuf::new();
    for comp in abs.components() {
        let s = comp.as_os_str().to_string_lossy();
        if has_glob_chars(&s) {
            break;
        }
        root.push(comp);
    }

    let mut found = Vec::new();
    if root.exists() {
        let mut budget = MAX_WALK_ENTRIES;
        walk_and_match(&root, &matcher, &mut found, &mut budget)?;
    }
    if found.is_empty() {
        // Nullglob off: the unmatched pattern passes through literally.
        return Ok(vec![abs]);
    }
    found.sort();
    Ok(found)
}

/// Bounded DFS collecting every path under `dir` (inclusive) that the
/// matcher accepts. Directory symlinks are not followed.
fn walk_and_match(
    dir: &Path,
    matcher: &globset::GlobMatcher,
    out: &mut Vec<PathBuf>,
    budget: &mut usize,
) -> Result<(), Unresolved> {
    let glob_too_broad = || {
        u(
            "glob-too-broad",
            "This glob is too broad to expand at hook time. Narrow the pattern or \
             list the files.",
        )
    };
    if *budget == 0 {
        return Err(glob_too_broad());
    }
    *budget -= 1;
    if matcher.is_match(dir) {
        out.push(dir.to_path_buf());
    }
    let is_dir_no_follow = std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir());
    if !is_dir_no_follow {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        walk_and_match(&entry.path(), matcher, out, budget)?;
    }
    Ok(())
}

/// Dir-ness of a resolved path, when queryable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DirQ {
    Yes,
    No,
    Unknown,
}

/// Query whether `path` is a directory. Relative paths (unknown host cwd)
/// are unqueryable.
fn query_is_dir(path: &Path) -> DirQ {
    if path.is_absolute() {
        if path.is_dir() { DirQ::Yes } else { DirQ::No }
    } else {
        DirQ::Unknown
    }
}

/// Enumerate every file under `root` (symlinks recorded as entries, not
/// followed), returned as paths relative to `root`. Bounded like globs.
fn walk_tree(root: &Path) -> Result<Vec<PathBuf>, Unresolved> {
    fn inner(
        dir: &Path,
        root: &Path,
        out: &mut Vec<PathBuf>,
        budget: &mut usize,
    ) -> Result<(), Unresolved> {
        if *budget == 0 {
            return Err(u(
                "tree-too-large",
                "This directory tree is too large to enumerate at hook time. Copy or \
                 sync a narrower directory.",
            ));
        }
        *budget -= 1;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let meta = std::fs::symlink_metadata(&p);
            if meta.as_ref().is_ok_and(std::fs::Metadata::is_dir) {
                inner(&p, root, out, budget)?;
            } else if let Ok(rel) = p.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    let mut budget = MAX_WALK_ENTRIES;
    inner(root, root, &mut out, &mut budget)?;
    Ok(out)
}

// ── Operand / flag splitting (argument-convention writers, layer 2) ──────────

/// Which flags a modeled tool accepts without changing its write-set shape.
/// Anything outside the table is Opaque (fail-closed): an unmodeled flag may
/// relocate writes (`cp -t`, `--backup`) and silently under-record.
struct FlagSpec {
    /// Boolean short flags, combinable (`-rf`).
    shorts: &'static str,
    /// Boolean long flags (no value).
    longs: &'static [&'static str],
    /// Long flags accepted only in `--name=value` form (value ignored).
    value_longs: &'static [&'static str],
}

/// Split argv into operands, validating flags against the spec.
fn split_operands<'a>(
    cmd: &'a SimpleCommand,
    spec: &FlagSpec,
    tool: &str,
) -> Result<Vec<(&'a str, WordMeta)>, Unresolved> {
    let unmodeled = |flag: &str| {
        u(
            "unmodeled-flag",
            format!(
                "`{tool} {flag}` changes where writes land in a way the resolver \
                 doesn't model, so the write-set can't be attributed. Use the plain \
                 `{tool} SRC… DST` form."
            ),
        )
    };
    let mut operands = Vec::new();
    let mut after_ddash = false;
    for (i, arg) in cmd.argv.iter().enumerate() {
        let meta = cmd.argv_meta.get(i).copied().unwrap_or_default();
        if after_ddash || arg == "-" || !arg.starts_with('-') {
            operands.push((arg.as_str(), meta));
            continue;
        }
        if arg == "--" {
            after_ddash = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            let (name, has_value) = long
                .split_once('=')
                .map_or((long, false), |(n, _)| (n, true));
            let ok = if has_value {
                spec.value_longs.contains(&name)
            } else {
                spec.longs.contains(&name)
            };
            if !ok {
                return Err(unmodeled(arg));
            }
            continue;
        }
        // Short cluster: every char must be a known boolean.
        for c in arg[1..].chars() {
            if !spec.shorts.contains(c) {
                return Err(unmodeled(&format!("-{c}")));
            }
        }
    }
    Ok(operands)
}

/// Expand an operand list-position word (sources, `tee`/`sed -i` files).
fn expand_list_operand(
    word: &str,
    meta: WordMeta,
    state: &State,
) -> Result<Vec<PathBuf>, Unresolved> {
    expand_word(word, meta, state, Position::List)
}

/// Expand a destination word to exactly one path.
fn expand_destination(word: &str, meta: WordMeta, state: &State) -> Result<PathBuf, Unresolved> {
    let mut paths = expand_word(word, meta, state, Position::Single)?;
    if paths.len() == 1 {
        Ok(paths.remove(0))
    } else {
        Err(u(
            "multi-word-destination",
            "This destination expands to several words (or to a device sink), so the \
             write-set can't be pinned. Name one destination path.",
        ))
    }
}

/// The final path component as a `PathBuf`, when one exists.
fn base_name(path: &Path) -> Option<PathBuf> {
    path.file_name().map(PathBuf::from)
}

// ── cp / mv (argument-convention movers) ─────────────────────────────────────

/// `cp [flags] SRC… DST` → the landing paths under `DST` (decision 026: the
/// landing is what is recorded, wherever the bytes come from —
/// `cp /dev/stdin f` included).
fn resolve_cp(cmd: &SimpleCommand, state: &State) -> Result<SegmentClass, Unresolved> {
    const CP_FLAGS: FlagSpec = FlagSpec {
        shorts: "arRfinvpLPHdusxl",
        longs: &[
            "recursive",
            "archive",
            "force",
            "interactive",
            "no-clobber",
            "verbose",
            "dereference",
            "no-dereference",
            "update",
            "one-file-system",
            "symbolic-link",
            "link",
            "preserve",
            "no-preserve",
        ],
        value_longs: &["preserve", "no-preserve", "reflink", "sparse"],
    };
    let recursive = cmd.argv.iter().any(|a| {
        a == "--recursive"
            || a == "--archive"
            || (a.starts_with('-') && !a.starts_with("--") && a.contains(['r', 'R', 'a']))
    });
    let operands = split_operands(cmd, &CP_FLAGS, "cp")?;
    let Some(((dst_word, dst_meta), srcs)) = operands.split_last() else {
        return Ok(SegmentClass::NoWrite);
    };
    if srcs.is_empty() {
        return Ok(SegmentClass::NoWrite);
    }
    let dst = expand_destination(dst_word, *dst_meta, state)?;
    let mut writes = BTreeSet::new();
    for (src_word, src_meta) in srcs {
        for src in expand_list_operand(src_word, *src_meta, state)? {
            mover_landings(&src, &dst, recursive, "cp", &mut writes)?;
        }
    }
    Ok(SegmentClass::Recorded(writes))
}

/// `mv [flags] SRC… DST` → the write of the destination side (the
/// disappearance of the source is a pure delete — coherence's).
fn resolve_mv(cmd: &SimpleCommand, state: &State) -> Result<SegmentClass, Unresolved> {
    const MV_FLAGS: FlagSpec = FlagSpec {
        shorts: "finuv",
        longs: &["force", "interactive", "no-clobber", "update", "verbose"],
        value_longs: &[],
    };
    let operands = split_operands(cmd, &MV_FLAGS, "mv")?;
    let Some(((dst_word, dst_meta), srcs)) = operands.split_last() else {
        return Ok(SegmentClass::NoWrite);
    };
    if srcs.is_empty() {
        return Ok(SegmentClass::NoWrite);
    }
    let dst = expand_destination(dst_word, *dst_meta, state)?;
    let mut writes = BTreeSet::new();
    for (src_word, src_meta) in srcs {
        for src in expand_list_operand(src_word, *src_meta, state)? {
            // mv moves directories without a flag — treat it as recursive.
            mover_landings(&src, &dst, true, "mv", &mut writes)?;
        }
    }
    Ok(SegmentClass::Recorded(writes))
}

/// Landing paths for one source of a `cp`/`mv`: dir destinations take
/// `DST/basename(src)`; directory sources enumerate their tree at hook time
/// (a state query; over-enumeration is over-recording, safe). Unqueryable
/// dir-ness records both interpretations for plain files and fails closed
/// for trees.
fn mover_landings(
    src: &Path,
    dst: &Path,
    recursive: bool,
    tool: &str,
    writes: &mut BTreeSet<PathBuf>,
) -> Result<(), Unresolved> {
    let need_cwd = || {
        u(
            "no-cwd-for-query",
            format!(
                "`{tool}` needs a filesystem query (directory enumeration) to resolve \
                 this write-set, but the hook has no working directory for the call. \
                 Use absolute paths."
            ),
        )
    };
    let src_is_dir = query_is_dir(src);
    match query_is_dir(dst) {
        DirQ::Yes => {
            let Some(base) = base_name(src) else {
                return Ok(());
            };
            let landing = dst.join(base);
            match src_is_dir {
                DirQ::Yes if recursive => {
                    writes.insert(landing.clone());
                    for rel in walk_tree(src)? {
                        writes.insert(landing.join(rel));
                    }
                }
                // A dir source without -r: cp refuses it — no write.
                DirQ::Yes => {}
                DirQ::No => {
                    writes.insert(landing);
                }
                DirQ::Unknown => return Err(need_cwd()),
            }
        }
        DirQ::No => match src_is_dir {
            DirQ::Yes if recursive => {
                writes.insert(dst.to_path_buf());
                for rel in walk_tree(src)? {
                    writes.insert(dst.join(rel));
                }
            }
            DirQ::Yes => {}
            DirQ::No => {
                writes.insert(dst.to_path_buf());
            }
            DirQ::Unknown => return Err(need_cwd()),
        },
        DirQ::Unknown => {
            if recursive {
                // A tree may land here and can't be enumerated — fail closed.
                return Err(need_cwd());
            }
            // Non-recursive: the source must be a plain file (a dir would
            // error), so the landing is one of exactly two paths — record
            // both (over-recording, the safe direction).
            writes.insert(dst.to_path_buf());
            if let Some(base) = base_name(src) {
                writes.insert(dst.join(base));
            }
        }
    }
    Ok(())
}

// ── tee ──────────────────────────────────────────────────────────────────────

/// `tee [flags] FILE…` → its file operands, mid-pipeline or terminal. Flags
/// never add targets, so they pass through unchecked.
fn resolve_tee(cmd: &SimpleCommand, state: &State) -> Result<SegmentClass, Unresolved> {
    let mut writes = BTreeSet::new();
    let mut after_ddash = false;
    for (i, arg) in cmd.argv.iter().enumerate() {
        if arg == "--" && !after_ddash {
            after_ddash = true;
            continue;
        }
        if !after_ddash && arg.starts_with('-') && arg != "-" {
            continue;
        }
        let meta = cmd.argv_meta.get(i).copied().unwrap_or_default();
        writes.extend(expand_list_operand(arg, meta, state)?);
    }
    Ok(SegmentClass::Recorded(writes))
}

// ── sed (script checked: pure editing subset only) ───────────────────────────

/// Parsed shape of a sed invocation.
struct SedInvocation {
    script: String,
    in_place: Option<String>,
    files: Vec<(String, WordMeta)>,
}

/// `sed -i` / `-i.bak` → its file arguments plus the backup side-write, with
/// the **script checked**: only the pure editing subset passes; `w`/`W`/`e`
/// (and `s///w`, `s///e`) are surgically denied naming the construct
/// (bug 12's lesson, fail-closed). A pure script without `-i` writes nothing.
fn resolve_sed(cmd: &SimpleCommand, state: &State) -> Result<SegmentClass, Unresolved> {
    let inv = parse_sed_argv(cmd)?;
    if inv.script.is_empty() {
        // No script: sed errors out before writing anything.
        return Ok(SegmentClass::NoWrite);
    }
    check_sed_script(&inv.script)?;
    let Some(suffix) = inv.in_place else {
        return Ok(SegmentClass::NoWrite);
    };
    if suffix.contains('*') || suffix.contains('/') {
        return Err(u(
            "sed-backup-template",
            "A `sed -i` backup suffix containing `*` or `/` is a GNU template whose \
             backup paths the resolver doesn't model. Use a plain suffix like \
             `-i.bak`.",
        ));
    }
    let mut writes = BTreeSet::new();
    for (file, meta) in &inv.files {
        for path in expand_list_operand(file, *meta, state)? {
            if !suffix.is_empty() {
                let mut backup = path.clone().into_os_string();
                backup.push(&suffix);
                writes.insert(PathBuf::from(backup));
            }
            writes.insert(path);
        }
    }
    Ok(SegmentClass::Recorded(writes))
}

/// Split a sed argv into script / `-i` suffix / file operands, failing
/// closed on anything that would smuggle an unseen script (`-f`, a computed
/// `-e`) or an unknown flag.
#[allow(
    clippy::too_many_lines,
    reason = "one linear argv scan over sed's flag grammar; splitting the \
              long/short/positional arms would scatter the shared script/file \
              accumulation state"
)]
fn parse_sed_argv(cmd: &SimpleCommand) -> Result<SedInvocation, Unresolved> {
    let unknown = |flag: &str| {
        u(
            "unmodeled-flag",
            format!(
                "`sed {flag}` isn't a flag the resolver models, so the script/file \
                 split can't be trusted. Use the plain `sed [-n] [-i[SUF]] SCRIPT \
                 FILE…` form."
            ),
        )
    };
    let computed_script = || {
        u(
            "computed-sed-script",
            "This sed script is computed at runtime (`$VAR` / `$(…)` / an unquoted \
             glob), so it can't be checked for write/exec commands. Quote a literal \
             script.",
        )
    };
    let mut scripts: Vec<String> = Vec::new();
    let mut files: Vec<(String, WordMeta)> = Vec::new();
    let mut in_place: Option<String> = None;
    let mut positional_script_taken = false;
    let mut after_ddash = false;
    let mut skip_next: bool = false;
    let mut next_is_script = false;

    for (i, arg) in cmd.argv.iter().enumerate() {
        let meta = cmd.argv_meta.get(i).copied().unwrap_or_default();
        if skip_next {
            skip_next = false;
            continue;
        }
        if next_is_script {
            next_is_script = false;
            if meta.value_subs || meta.live_dollar || meta.live_glob {
                return Err(computed_script());
            }
            scripts.push(arg.clone());
            continue;
        }
        if after_ddash {
            files.push((arg.clone(), meta));
            continue;
        }
        if arg == "--" {
            after_ddash = true;
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            match long {
                "in-place" => in_place = Some(String::new()),
                "expression" => next_is_script = true,
                "quiet" | "silent" | "regexp-extended" | "separate" | "null-data" | "posix"
                | "unbuffered" | "debug" | "sandbox" | "follow-symlinks" => {}
                _ if long.starts_with("in-place=") => {
                    in_place = Some(long["in-place=".len()..].to_string());
                }
                _ if long.starts_with("expression=") => {
                    if meta.value_subs || meta.live_dollar || meta.live_glob {
                        return Err(computed_script());
                    }
                    scripts.push(long["expression=".len()..].to_string());
                }
                _ if long.starts_with("line-length=") => {}
                _ if long == "file" || long.starts_with("file=") => {
                    return Err(u(
                        "sed-script-file",
                        "`sed -f`/`--file` reads the script from a file the hook can't \
                         check for write/exec commands. Inline the script.",
                    ));
                }
                _ => return Err(unknown(arg)),
            }
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            let cluster = &arg[1..];
            let mut handled_rest = false;
            for (off, c) in cluster.char_indices() {
                match c {
                    'n' | 'E' | 'r' | 's' | 'z' | 'u' => {}
                    'i' => {
                        in_place = Some(cluster[off + 1..].to_string());
                        handled_rest = true;
                    }
                    'e' => {
                        let rest = &cluster[off + 1..];
                        if rest.is_empty() {
                            next_is_script = true;
                        } else {
                            if meta.value_subs || meta.live_dollar || meta.live_glob {
                                return Err(computed_script());
                            }
                            scripts.push(rest.to_string());
                        }
                        handled_rest = true;
                    }
                    'f' => {
                        return Err(u(
                            "sed-script-file",
                            "`sed -f`/`--file` reads the script from a file the hook \
                             can't check for write/exec commands. Inline the script.",
                        ));
                    }
                    'l' => {
                        if cluster[off + 1..].is_empty() {
                            skip_next = true;
                        }
                        handled_rest = true;
                    }
                    _ => return Err(unknown(&format!("-{c}"))),
                }
                if handled_rest {
                    break;
                }
            }
            continue;
        }
        if !positional_script_taken && scripts.is_empty() {
            if meta.value_subs || meta.live_dollar || meta.live_glob {
                return Err(computed_script());
            }
            scripts.push(arg.clone());
            positional_script_taken = true;
        } else {
            files.push((arg.clone(), meta));
        }
    }
    Ok(SedInvocation {
        script: scripts.join("\n"),
        in_place,
        files,
    })
}

/// Verify a sed script parses into the pure editing subset. Anything the
/// parser cannot positively account for is denied — the script is parsed,
/// not trusted (soundness layer 4).
///
/// Denied constructs, named: `w`/`W` (write to file), `e` (execute), the
/// `s///w` and `s///e` flags.
fn check_sed_script(script: &str) -> Result<(), Unresolved> {
    let unverifiable = || {
        u(
            "sed-unverifiable-script",
            "This sed script couldn't be verified as pure editing (the checked subset: \
             `s///`, `y///`, addresses, `d p n N h H g G x q b t : { }` and `a i c` \
             text). Simplify or split the script.",
        )
    };
    let bytes = script.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Separators / whitespace between commands.
        if bytes[i].is_ascii_whitespace() || bytes[i] == b';' {
            i += 1;
            continue;
        }
        // Addresses: numbers (with ~ steps), $, /regex/, \cREGEXc — up to two,
        // comma/`+`/`~`-joined, optional `!`.
        i = skip_sed_addresses(bytes, i).ok_or_else(unverifiable)?;
        if i >= bytes.len() {
            return Err(unverifiable());
        }
        let c = bytes[i];
        i += 1;
        match c {
            b'w' | b'W' => {
                return Err(u(
                    "sed-write-command",
                    "The sed `w`/`W` command writes to a file named inside the script — \
                     an unattributable write. Use `sed -i` on the target files, or a \
                     redirect, both of which resolve.",
                ));
            }
            b'e' => {
                return Err(u(
                    "sed-exec-command",
                    "The GNU sed `e` command executes a shell command from inside the \
                     script — unresolvable. Run the command directly.",
                ));
            }
            b's' => {
                i = check_sed_substitution(bytes, i).map_err(|constr| match constr {
                    SedSubErr::WriteFlag => u(
                        "sed-s-write-flag",
                        "The sed `s///w FILE` flag writes to a file named inside the \
                         script — an unattributable write. Drop the `w` flag; use \
                         `sed -i` or a redirect instead.",
                    ),
                    SedSubErr::ExecFlag => u(
                        "sed-s-exec-flag",
                        "The GNU sed `s///e` flag executes the pattern space as a shell \
                         command — unresolvable. Drop the `e` flag and run the command \
                         directly.",
                    ),
                    SedSubErr::Unparseable => unverifiable(),
                })?;
            }
            b'y' => {
                i = skip_sed_delimited(bytes, i, 2).ok_or_else(unverifiable)?;
            }
            b'q' | b'Q' => {
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            b'l' => {
                while i < bytes.len() && (bytes[i] == b' ' || bytes[i].is_ascii_digit()) {
                    i += 1;
                }
            }
            b'd' | b'D' | b'p' | b'P' | b'n' | b'N' | b'h' | b'H' | b'g' | b'G' | b'x' | b'='
            | b'z' | b'F' | b'{' | b'}' => {}
            b'b' | b't' | b'T' | b':' => {
                // Label runs to `;` or newline.
                while i < bytes.len() && bytes[i] != b';' && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'a' | b'i' | b'c' => {
                // GNU one-line text: runs to end of line (`;` does not end it).
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'r' | b'R' => {
                // Read-file commands: filename runs to end of line; reads only.
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            _ => return Err(unverifiable()),
        }
    }
    Ok(())
}

/// Skip optional sed addresses before a command. Returns the index of the
/// command letter, or `None` when the address syntax is unrecognized.
fn skip_sed_addresses(bytes: &[u8], mut i: usize) -> Option<usize> {
    let mut seen = 0;
    loop {
        let start = i;
        if i < bytes.len() && bytes[i].is_ascii_digit() {
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'~' {
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
        } else if i < bytes.len() && bytes[i] == b'$' {
            i += 1;
        } else if i < bytes.len() && bytes[i] == b'/' {
            i = skip_sed_regex(bytes, i + 1, b'/')?;
        } else if i + 1 < bytes.len() && bytes[i] == b'\\' {
            let delim = bytes[i + 1];
            i = skip_sed_regex(bytes, i + 2, delim)?;
        }
        if i == start {
            // No address here.
            break;
        }
        seen += 1;
        if seen > 2 {
            return None;
        }
        // Whitespace after an address.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        // Second address / offset forms.
        if i < bytes.len() && (bytes[i] == b',' || bytes[i] == b'+' || bytes[i] == b'~') {
            i += 1;
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            continue;
        }
        break;
    }
    // Negation(s).
    while i < bytes.len() && bytes[i] == b'!' {
        i += 1;
    }
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    Some(i)
}

/// Skip a delimited sed regex starting just after its opening delimiter,
/// honoring backslash escapes. Returns the index just past the closing
/// delimiter.
fn skip_sed_regex(bytes: &[u8], mut i: usize, delim: u8) -> Option<usize> {
    while i < bytes.len() {
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

/// Skip `count` delimited sections (`/re/repl/` style) starting at the
/// delimiter character itself.
fn skip_sed_delimited(bytes: &[u8], mut i: usize, count: usize) -> Option<usize> {
    if i >= bytes.len() {
        return None;
    }
    let delim = bytes[i];
    i += 1;
    for _ in 0..count {
        i = skip_sed_regex(bytes, i, delim)?;
    }
    Some(i)
}

/// Why an `s///` command failed the pure-subset check.
enum SedSubErr {
    WriteFlag,
    ExecFlag,
    Unparseable,
}

/// Check one `s` command starting at its delimiter: scan pattern and
/// replacement, then vet the flags (`g i I m M p` + digits pass; `w` and `e`
/// are the denied writers/executors).
fn check_sed_substitution(bytes: &[u8], i: usize) -> Result<usize, SedSubErr> {
    let mut j = skip_sed_delimited(bytes, i, 2).ok_or(SedSubErr::Unparseable)?;
    while j < bytes.len() {
        match bytes[j] {
            b'g' | b'i' | b'I' | b'm' | b'M' | b'p' | b'0'..=b'9' => j += 1,
            b'w' => return Err(SedSubErr::WriteFlag),
            b'e' => return Err(SedSubErr::ExecFlag),
            b';' | b'\n' | b' ' | b'\t' | b'}' => break,
            _ => return Err(SedSubErr::Unparseable),
        }
    }
    Ok(j)
}

// ── rsync (source-enumerated superset) ───────────────────────────────────────

/// `rsync SRC… DST` (local) → the target tree enumerated from the sources at
/// hook time; over-enumeration is over-recording (rsync skips identical
/// files), the safe direction. Remote endpoints are Opaque until modeled;
/// `--delete` is a pure delete — free.
fn resolve_rsync(cmd: &SimpleCommand, state: &State) -> Result<SegmentClass, Unresolved> {
    const RSYNC_FLAGS: FlagSpec = FlagSpec {
        shorts: "avqzrlptgoDHAXnPucihW",
        longs: &[
            "archive",
            "verbose",
            "quiet",
            "compress",
            "recursive",
            "links",
            "perms",
            "times",
            "group",
            "owner",
            "devices",
            "specials",
            "dry-run",
            "partial",
            "progress",
            "checksum",
            "update",
            "inplace",
            "whole-file",
            "human-readable",
            "itemize-changes",
            "stats",
            "mkpath",
            "delete",
            "delete-after",
            "delete-before",
            "delete-during",
            "delete-excluded",
        ],
        value_longs: &[
            "exclude",
            "include",
            "filter",
            "exclude-from",
            "include-from",
            "max-size",
            "min-size",
            "bwlimit",
            "timeout",
            "chmod",
            "out-format",
            "info",
            "debug",
        ],
    };
    // files-from changes the source set itself — fail closed before the
    // generic flag check so the message names the construct.
    if cmd
        .argv
        .iter()
        .any(|a| a == "--files-from" || a.starts_with("--files-from="))
    {
        return Err(u(
            "rsync-files-from",
            "`rsync --files-from` takes the transfer list from a file at runtime, so \
             the write-set can't be resolved. Pass the sources on the command line.",
        ));
    }
    let operands = split_operands(cmd, &RSYNC_FLAGS, "rsync")?;
    for (op, _) in &operands {
        if is_remote_rsync_operand(op) {
            return Err(u(
                "rsync-remote",
                "Remote rsync endpoints aren't modeled — the hook can't enumerate the \
                 write-set. Run local-to-local rsync, or copy explicitly.",
            ));
        }
    }
    let Some(((dst_word, dst_meta), srcs)) = operands.split_last() else {
        return Ok(SegmentClass::NoWrite);
    };
    if srcs.is_empty() {
        // Single-operand rsync just lists.
        return Ok(SegmentClass::NoWrite);
    }
    let dst = expand_destination(dst_word, *dst_meta, state)?;
    let dst_is_dirish = query_is_dir(&dst) == DirQ::Yes || dst_word.ends_with('/');
    let mut writes = BTreeSet::new();
    for (src_word, src_meta) in srcs {
        for src in expand_list_operand(src_word, *src_meta, state)? {
            rsync_landings(
                &src,
                src_word.ends_with('/'),
                &dst,
                dst_is_dirish || srcs.len() > 1,
                &mut writes,
            )?;
        }
    }
    Ok(SegmentClass::Recorded(writes))
}

/// Whether an rsync operand names a remote endpoint (`host:path`,
/// `user@host:path`, `rsync://…`, `host::module`).
fn is_remote_rsync_operand(op: &str) -> bool {
    if op.starts_with("rsync://") || op.contains("::") {
        return true;
    }
    op.find(':').is_some_and(|colon| !op[..colon].contains('/'))
}

/// Landing paths for one rsync source: a trailing slash syncs *contents*
/// into DST; without it the source directory itself lands under DST.
fn rsync_landings(
    src: &Path,
    src_trailing_slash: bool,
    dst: &Path,
    dst_is_dirish: bool,
    writes: &mut BTreeSet<PathBuf>,
) -> Result<(), Unresolved> {
    match query_is_dir(src) {
        DirQ::Yes => {
            let landing = if src_trailing_slash {
                dst.to_path_buf()
            } else {
                base_name(src).map_or_else(|| dst.to_path_buf(), |b| dst.join(b))
            };
            writes.insert(landing.clone());
            for rel in walk_tree(src)? {
                writes.insert(landing.join(rel));
            }
            Ok(())
        }
        DirQ::No => {
            if !dst_is_dirish {
                writes.insert(dst.to_path_buf());
            }
            if let Some(base) = base_name(src) {
                writes.insert(dst.join(base));
            }
            Ok(())
        }
        DirQ::Unknown => Err(u(
            "no-cwd-for-query",
            "`rsync` enumerates the write-set from its sources — a filesystem query — \
             but the hook has no working directory for the call. Use absolute paths.",
        )),
    }
}

// ── Shell wrappers over a literal program ────────────────────────────────────

/// `bash -c 'literal program'` → recurse into the program like any subshell
/// (fresh bindings — the inner shell inherits the environment, not this
/// line's locals; same cwd). `bash -c "$PROG"` and every computed program is
/// Opaque. Without `-c` (a script file), program-internal writes keep the
/// inherited layer-4 boundary: `NoWrite`.
fn resolve_shell_wrapper(cmd: &SimpleCommand, state: &State, seg_name: &str) -> SegmentClass {
    let mut i = 0;
    let mut has_c = false;
    while i < cmd.argv.len() {
        let arg = &cmd.argv[i];
        if !arg.starts_with('-') || arg == "-" || arg == "--" {
            break;
        }
        if arg.starts_with("--") || arg == "-o" {
            // Long flags (--rcfile FILE, -o opt) can take values — the
            // program-argument position can't be trusted past them.
            if cmd.argv.iter().any(|a| a == "-c") || arg.contains('c') {
                return SegmentClass::Opaque(
                    u(
                        "unmodeled-shell-flag",
                        format!(
                            "`{seg_name} {arg}` isn't a flag the resolver models, so it \
                             can't locate the `-c` program to check. Use the plain \
                             `{seg_name} -c 'program'` form."
                        ),
                    )
                    .into_opaque(seg_name),
                );
            }
            i += 1;
            continue;
        }
        if arg[1..].chars().all(|c| "lexuips".contains(c) || c == 'c') {
            if arg.contains('c') {
                has_c = true;
                i += 1;
                break;
            }
            i += 1;
            continue;
        }
        // Unknown short cluster before the program position.
        return SegmentClass::Opaque(
            u(
                "unmodeled-shell-flag",
                format!(
                    "`{seg_name} {arg}` isn't a flag the resolver models, so it can't \
                     locate the `-c` program to check. Use the plain `{seg_name} -c \
                     'program'` form."
                ),
            )
            .into_opaque(seg_name),
        );
    }
    if !has_c {
        // Script-file / interactive invocation: program-internal writes keep
        // the inherited accepted boundary (soundness layer 4).
        return SegmentClass::NoWrite;
    }
    // A `--` between `-c` and the program is an option terminator.
    if cmd.argv.get(i).is_some_and(|a| a == "--") {
        i += 1;
    }
    let Some(program) = cmd.argv.get(i) else {
        return SegmentClass::NoWrite;
    };
    let meta = cmd.argv_meta.get(i).copied().unwrap_or_default();
    if meta.value_subs || meta.any_live() {
        return SegmentClass::Opaque(
            u(
                "computed-shell-program",
                format!(
                    "`{seg_name} -c` with a computed program (`\"$PROG\"`, `$(…)`, an \
                     unquoted glob) can't be checked, so any write inside it would go \
                     unattributed. Quote a literal program: `{seg_name} -c 'echo x > \
                     f'`."
                ),
            )
            .into_opaque(seg_name),
        );
    }
    // Recurse: the program is static text — parse and resolve it with fresh
    // bindings (this line's locals are not exported) and the same cwd.
    let script = parse::parse(program);
    let mut inner_state = State {
        cwd: state.cwd.clone(),
        bindings: HashMap::new(),
        vars_tainted: false,
    };
    let mut writes = BTreeSet::new();
    match resolve_into(&script, &mut inner_state, &mut writes) {
        Ok(()) => SegmentClass::Recorded(writes),
        Err(op) => SegmentClass::Opaque(op),
    }
}

// ── xargs (stdin-driven target lists) ────────────────────────────────────────

/// Build a synthetic single-command from an `xargs`-wrapped command's tail (the
/// argv slots from `from` onward), so a checkable writer's own program (a `sed`
/// script, an `awk`/`perl` program) can be resolved as if invoked directly.
fn wrapped_command(cmd: &SimpleCommand, from: usize, name: &str) -> SimpleCommand {
    SimpleCommand {
        name: Some(name.to_string()),
        argv: cmd.argv.get(from..).unwrap_or_default().to_vec(),
        argv_meta: cmd.argv_meta.get(from..).unwrap_or_default().to_vec(),
        ..SimpleCommand::default()
    }
}

/// `xargs [flags] CMD…`: the wrapped command's extra arguments arrive on
/// stdin at runtime. A wrapped registry writer therefore has an unresolvable
/// target list — Opaque; a wrapped filter/reader is `NoWrite`; `rm` stays a
/// pure delete (unknown deletions carry no debt).
#[allow(
    clippy::too_many_lines,
    reason = "one linear scan over xargs's flag grammar plus the wrapped-command \
              dispatch; splitting it would scatter the shared stdin_targets \
              closure across helpers"
)]
fn resolve_xargs(cmd: &SimpleCommand, state: &State, seg_name: &str) -> SegmentClass {
    let stdin_targets = |wrapped: &str| {
        SegmentClass::Opaque(
            u(
                "stdin-driven-targets",
                format!(
                    "`xargs {wrapped}` takes its write targets from stdin at runtime, \
                     so they can't be attributed. Materialize the list (e.g. `catenary \
                     grep -l PAT`), then pass the paths as literal arguments."
                ),
            )
            .into_opaque(seg_name),
        )
    };
    let mut i = 0;
    while i < cmd.argv.len() {
        let arg = &cmd.argv[i];
        if arg == "--" {
            i += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        match arg.as_str() {
            "-0" | "-r" | "-t" | "-p" | "-x" | "-o" | "--null" | "--no-run-if-empty"
            | "--verbose" | "--exit" | "--open-tty" => i += 1,
            "-a" | "-d" | "-E" | "-e" | "-I" | "-L" | "-l" | "-n" | "-P" | "-s" => i += 2,
            _ if arg.starts_with("--") && arg.contains('=') => i += 1,
            // Glued value shorts (`-n1`, `-I{}`, `-i{}`, `-d\n`).
            _ if arg.len() > 2
                && matches!(
                    &arg[..2],
                    "-a" | "-d" | "-E" | "-e" | "-I" | "-i" | "-L" | "-l" | "-n" | "-P" | "-s"
                ) =>
            {
                i += 1;
            }
            "-i" => i += 1,
            _ => {
                return SegmentClass::Opaque(
                    u(
                        "unmodeled-flag",
                        format!(
                            "`xargs {arg}` isn't a flag the resolver models, so it \
                             can't locate the wrapped command to check. Use plain \
                             `xargs CMD` — or materialize the list and pass literal \
                             paths."
                        ),
                    )
                    .into_opaque(seg_name),
                );
            }
        }
    }
    let Some(wrapped_word) = cmd.argv.get(i) else {
        // Bare xargs wraps echo — a filter.
        return SegmentClass::NoWrite;
    };
    let wrapped = Path::new(wrapped_word).file_name().map_or_else(
        || wrapped_word.clone(),
        |n| n.to_string_lossy().into_owned(),
    );
    match wrapped.as_str() {
        "rm" | "rmdir" => SegmentClass::PureDelete,
        "sed" => {
            // `-i` under xargs edits stdin-named files; without it the script
            // must still pass the purity check.
            let rest = &cmd.argv[i + 1..];
            if rest.iter().any(|a| {
                a == "-i"
                    || a.starts_with("--in-place")
                    || (a.starts_with('-') && !a.starts_with("--") && a.contains('i'))
            }) {
                return stdin_targets("sed -i");
            }
            run(
                resolve_sed(&wrapped_command(cmd, i + 1, "sed"), state),
                seg_name,
            )
        }
        "awk" | "gawk" | "mawk" | "nawk" => {
            // gawk `-i inplace` under xargs edits stdin-named files; a pure
            // filter still has its program checked for exec/redirect hazards.
            let rest = &cmd.argv[i + 1..];
            if rest.iter().any(|a| {
                a.starts_with("--include")
                    || (a.starts_with('-') && !a.starts_with("--") && a.contains('i'))
            }) {
                return stdin_targets(&format!("{wrapped} -i inplace"));
            }
            let inner = wrapped_command(cmd, i + 1, &wrapped);
            run(programs::resolve_awk(&inner, state), seg_name)
        }
        "perl" => {
            // `-i` under xargs edits stdin-named files; a pure filter still has
            // its substitution program checked.
            let rest = &cmd.argv[i + 1..];
            if rest
                .iter()
                .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains('i'))
            {
                return stdin_targets("perl -i");
            }
            let inner = wrapped_command(cmd, i + 1, "perl");
            run(programs::resolve_perl(&inner, state), seg_name)
        }
        "cp" | "mv" | "tee" | "rsync" | "dd" | "install" | "truncate" | "bash" | "sh" | "zsh"
        | "dash" | "ksh" | "eval" | "source" | "xargs" => stdin_targets(&wrapped),
        _ => SegmentClass::NoWrite,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::literal_string_with_formatting_args,
    reason = "tests use expect/panic for readable assertion failures; shell \
              `${var}` forms in command strings are not format args"
)]
mod tests;
