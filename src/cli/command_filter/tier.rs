// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Command tiers — stateful operations are kitchen operations (root-ownership
//! stage 5).
//!
//! The durable root lock guards the edit seam and diagnostics; a second agent
//! running `git commit` or `make check` in someone else's kitchen is the same
//! trespass through a different door. Three tiers map the whole shell surface
//! onto the lock:
//!
//! - **Read** — open to any waiter, any window: everything in `allow`/`pipeline`
//!   without a write-set (`cat`, `diff`, `git log`/`status`/`diff`, the digest
//!   family, …). A waiter looks through the window unbounded — no lock is ever
//!   consulted for a read.
//! - **Edit** — lock required: exactly what the write resolver already
//!   identifies (`sed -i`, `cp`, `mv`, `rm`, `tee`, redirects, …). This tier is
//!   the resolved write-set the edit seam already gates; tier classification only
//!   *names* it so the three tiers form one exhaustive partition.
//! - **Stateful** — lock required: the `build` command and the mutating
//!   subcommands of allowed tools (`git commit`/`checkout`/`stash`/`merge`/
//!   `rebase`; `chmod`). These mutate the kitchen's committed/working state
//!   without necessarily resolving a write-set, so the write gate alone would let
//!   them through. The per-subcommand classification machinery already exists
//!   (`deny.git = [grep, ls-files, ls-tree]` proves it); the stateful set is the
//!   same shape of data, an annotation on the grant lists rather than new grant
//!   syntax.
//!
//! # Tier data rides the grant lists
//!
//! Tiers are DATA, not config: the classification reads the already-resolved
//! [`ResolvedCommands`](crate::config::ResolvedCommands) (the `build` set) plus a
//! static per-command mutating-subcommand table ([`MUTATING_SUBCOMMANDS`]).
//! Config and template meaning are untouched — a user never writes a tier, and
//! the recommended template is byte-identical to before. The stateful set is the
//! design's, keyed by the same command names the grant lists carry.

use std::path::Path;

use super::parse::{self, SimpleCommand};
use crate::config::ResolvedCommands;
use crate::lock::ReconcileDirection;

/// The lock tier of a shell command line.
///
/// The three tiers partition every command: [`Read`](Tier::Read) passes any
/// window, [`Edit`](Tier::Edit) and [`Stateful`](Tier::Stateful) both require
/// holding the root lock. The distinction between Edit and Stateful is
/// classificatory only — both gate identically — but keeps the taxonomy honest:
/// Edit is the write resolver's set, Stateful is the mutating-operation set the
/// write resolver does not model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Open to any waiter, any window — no lock consulted.
    Read,
    /// A resolved write (the edit seam's set). Lock required.
    Edit,
    /// A stateful mutating operation (`build`, mutating git subcommands,
    /// `chmod`). Lock required.
    Stateful,
}

impl Tier {
    /// Whether this tier requires holding the root lock.
    ///
    /// Read passes unconditionally; Edit and Stateful are lock-gated.
    #[must_use]
    pub const fn requires_lock(self) -> bool {
        matches!(self, Self::Edit | Self::Stateful)
    }
}

/// The mutating subcommands of allowed tools that lift a command to the
/// [`Stateful`](Tier::Stateful) tier — the same per-command data shape
/// `deny.<cmd>` carries, an annotation on the grant lists.
///
/// `git` commit/checkout/stash/merge/rebase move committed or working state
/// without a write-set the resolver can attribute (a commit records the index; a
/// checkout/stash/merge/rebase moves the working tree — the reconcile bracket is
/// how their disk effect books/unbooks the ledger). The list is deliberately the
/// mutating verbs from the DESIGN, not an exhaustive git surface: a read-only
/// subcommand (`git log`/`status`/`diff`) is never here, so it stays Read.
///
/// The set is a static approximation and grows fail-closed: an unlisted mutating
/// git verb classifies Read (a lone cook is never false-denied), which is the
/// safe direction — the edit seam and the reconcile bracket still catch any
/// actual write it performs.
const MUTATING_SUBCOMMANDS: &[(&str, &[&str])] =
    &[("git", &["commit", "checkout", "stash", "merge", "rebase"])];

/// Commands that are stateful on their own name, with no subcommand — the
/// mutating verbs whose whole invocation is a kitchen operation.
///
/// `chmod` mutates file mode (working-tree state) without a content write-set.
/// `build` is not here — it is resolved dynamically from
/// [`ResolvedCommands::build_for_cwd`], since the build tool is per-root config.
const STATEFUL_COMMANDS: &[&str] = &["chmod"];

/// Whether a command name at a given argv is a stateful mutating operation.
///
/// A command is stateful when: it is a `build` tool for the cwd; it is a
/// bare-name stateful command ([`STATEFUL_COMMANDS`], e.g. `chmod`); or it names
/// a tool with a mutating subcommand ([`MUTATING_SUBCOMMANDS`], e.g. `git
/// commit`) and its first non-flag argument is in that tool's mutating set.
fn command_is_stateful(
    name: &str,
    argv: &[String],
    rules: &ResolvedCommands,
    cwd: Option<&Path>,
) -> bool {
    // The build tool for this cwd (per-root, with the user-default fallback) is
    // a stateful kitchen operation — `make check`, `cargo build`, … mutate build
    // state and can touch covered files.
    if rules.build_for_cwd(cwd).iter().any(|t| t == name) {
        return true;
    }
    if STATEFUL_COMMANDS.contains(&name) {
        return true;
    }
    // A mutating subcommand of an allowed tool: the subcommand is the first
    // positional past any global options ([`git_subcommand`] skips git's
    // value-carrying globals like `-c key=val`, so `git -c core.editor=… commit`
    // still reads `commit`).
    if let Some(subs) = MUTATING_SUBCOMMANDS
        .iter()
        .find(|(cmd, _)| *cmd == name)
        .map(|(_, subs)| *subs)
        && let Some((sub, _)) = git_subcommand(argv)
    {
        return subs.contains(&sub.as_str());
    }
    false
}

/// The subcommand of a `git` argv (the first positional past git's global
/// options) and the index of its first following argument.
///
/// Mirrors the write resolver's `split_git` option-skip so a value-carrying
/// global (`-c key=val`, `-C dir`, `--git-dir foo`, `--namespace ns`) is stepped
/// over instead of being mistaken for the subcommand. Returns `None` for bare
/// `git` (only globals, no subcommand). A `--flag=value` global carries its own
/// value, so it consumes one token; the bare-word globals consume two.
fn git_subcommand(argv: &[String]) -> Option<(String, usize)> {
    let mut i = 0;
    while i < argv.len() {
        let a = argv[i].as_str();
        if !a.starts_with('-') {
            return Some((a.to_string(), i + 1));
        }
        match a {
            // Value-carrying globals consume the following token.
            "-C" | "--git-dir" | "--work-tree" | "-c" | "--namespace" | "--exec-path"
            | "--super-prefix" => i += 2,
            // `--opt=value` carries its own value; every other option consumes
            // one token.
            _ => i += 1,
        }
    }
    None
}

/// Classify a shell command line into its lock [`Tier`].
///
/// `writes` is the write resolver's resolved write-set for the line (computed by
/// the caller; a non-empty set is the [`Edit`](Tier::Edit) tier). The command
/// text is parsed to walk its command positions for stateful classification. A
/// line is:
///
/// - [`Stateful`](Tier::Stateful) when ANY command position (across pipelines and
///   substitutions) is a stateful mutating operation — the highest tier wins, so
///   a `git commit` anywhere in a chain lifts the whole line.
/// - [`Edit`](Tier::Edit) when the resolver produced any writes and no position
///   was stateful.
/// - [`Read`](Tier::Read) otherwise.
///
/// Stateful outranks Edit so a stateful command that also happens to resolve a
/// write (rare) gates and reconciles as stateful. The classification never
/// denies — it only labels; the caller applies the lock gate.
#[must_use]
pub fn classify_command(
    command: &str,
    writes: &[std::path::PathBuf],
    rules: &ResolvedCommands,
    cwd: Option<&Path>,
) -> Tier {
    let script = parse::parse(command);
    if script_has_stateful(&script, rules, cwd) {
        return Tier::Stateful;
    }
    if writes.is_empty() {
        Tier::Read
    } else {
        Tier::Edit
    }
}

/// Whether any command position in a parsed script (including substitutions) is
/// a stateful mutating operation.
fn script_has_stateful(
    script: &parse::ParsedScript,
    rules: &ResolvedCommands,
    cwd: Option<&Path>,
) -> bool {
    script
        .pipelines
        .iter()
        .flat_map(|p| p.commands.iter())
        .any(|cmd| command_is_stateful_recursive(cmd, rules, cwd))
}

/// Whether a command — or any command nested in its substitutions — is stateful.
fn command_is_stateful_recursive(
    command: &SimpleCommand,
    rules: &ResolvedCommands,
    cwd: Option<&Path>,
) -> bool {
    if let Some(name) = command.name.as_deref()
        && command_is_stateful(name, &command.argv, rules, cwd)
    {
        return true;
    }
    command
        .substitutions
        .iter()
        .any(|sub| script_has_stateful(sub, rules, cwd))
}

/// What the reconcile bracket does after a stateful-tier git command
/// (root-ownership stage 5; the merge arm narrowed by wf-01).
///
/// Most reconciling git commands drive the status-oracle ledger reconcile in a
/// [`ReconcileDirection`]. `git merge` is different in kind: debt means "an
/// agent edited this and nobody has looked," not "content moved" (the
/// pull-parity ruling), so a merge books nothing by itself — it only TRANSFERS
/// still-unpaid debt from a merged-from agent worktree's ledger into the owning
/// root's ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// Reconcile the cwd root's ledger against the `git status --porcelain`
    /// oracle in the given direction (`stash`/`checkout`/`rebase`, and
    /// `merge --abort` — the abort restores the pre-merge tree, so entries a
    /// transfer booked for the aborted merge go clean and unbook with it).
    Ledger(ReconcileDirection),
    /// A `git merge`: the worktree-debt transfer bracket (wf-01). `sources`
    /// are the merge's non-flag operands — the candidate merged-from refs.
    /// Every merge whose source is NOT an agent worktree root books nothing
    /// (pull-parity); an operand-less form (`--continue`/`--quit`) carries an
    /// empty `sources` and transfers nothing.
    MergeTransfer {
        /// The merged-from refs named on the command line.
        sources: Vec<String>,
    },
}

/// The reconcile action of a stateful-tier git command line, or `None` when
/// the line drives no reconcile (not a git command, a non-reconciling git
/// subcommand like `commit`, or an unrecognized shape).
///
/// Walks the top-level pipeline commands for a `git` word and classifies its
/// subcommand. Only the FIRST reconciling git command in the line is honored —
/// the bracket wraps one command per tool call, so a chain (denied at the
/// isolation/allowlist gates for other reasons in practice) never needs two
/// reconciles. `stash` bifurcates: `stash pop` / `stash apply` book (they
/// restore modifications), bare `stash` / `stash push` unbook (they remove
/// them). `merge` classifies as [`ReconcileAction::MergeTransfer`] — the
/// unconditional Book-on-merge retired with wf-01 (a `git pull` never booked
/// debt; a merge must not either).
#[must_use]
pub fn git_reconcile_action(command: &str) -> Option<ReconcileAction> {
    let script = parse::parse(command);
    for pipeline in &script.pipelines {
        for cmd in &pipeline.commands {
            if cmd.name.as_deref() != Some("git") {
                continue;
            }
            if let Some(action) = git_subcommand_action(&cmd.argv) {
                return Some(action);
            }
        }
    }
    None
}

/// Classify a `git` argv's reconcile action by its subcommand and, for
/// `stash`/`merge`, its operands.
fn git_subcommand_action(argv: &[String]) -> Option<ReconcileAction> {
    let (sub, rest_start) = git_subcommand(argv)?;
    match sub.as_str() {
        // Working-tree movers that REMOVE modifications: files that go clean are
        // unbooked. `checkout` here is the pathspec/undo restore (a covered file
        // reverted to its committed bytes leaves the gate); a branch switch moves
        // nothing dirty, so the reconcile simply finds nothing newly clean.
        "checkout" => Some(ReconcileAction::Ledger(ReconcileDirection::Unbook)),
        // `stash` bifurcates on its operand: pop/apply RESTORE modifications
        // (book), bare stash / push / save REMOVE them (unbook). The operand is
        // the first non-flag token after `stash`.
        "stash" => {
            let operand = argv[rest_start..].iter().find(|a| !a.starts_with('-'));
            Some(ReconcileAction::Ledger(match operand.map(String::as_str) {
                Some("pop" | "apply") => ReconcileDirection::Book,
                _ => ReconcileDirection::Unbook,
            }))
        }
        // `rebase` replays the agent's OWN commits, so the status-oracle Book
        // leg stands: a completed rebase leaves the tree clean (books nothing);
        // an interrupted one leaves the agent's restored modifications due.
        "rebase" => Some(ReconcileAction::Ledger(ReconcileDirection::Book)),
        // `merge` transfers unpaid worktree debt only (wf-01). `--abort`
        // restores the pre-merge tree, so it reconciles like a stash: due files
        // git now reports clean — including entries the merge's transfer just
        // booked — unbook.
        "merge" => {
            let rest = &argv[rest_start..];
            if rest.iter().any(|a| a == "--abort") {
                return Some(ReconcileAction::Ledger(ReconcileDirection::Unbook));
            }
            Some(ReconcileAction::MergeTransfer {
                sources: merge_source_operands(rest),
            })
        }
        // `commit` and every other git subcommand drive no reconcile.
        _ => None,
    }
}

/// The merged-from refs named on a `git merge` argv tail — its non-flag
/// operands, with the values of value-carrying merge options stepped over.
///
/// `git merge -m "msg" topic` must yield `topic`, not the message: the listed
/// separated-value options (`-m`, `-F`, `-s`, `-X` and their long forms)
/// consume the following token. A `--opt=value` form carries its own value and
/// is skipped as a flag. Everything after a literal `--` is operands (merge
/// takes no pathspec, so the tail is refs). An unlisted value-carrying option
/// mis-reads its value as a ref — the safe direction: a bogus ref resolves to
/// no worktree and transfers nothing.
fn merge_source_operands(rest: &[String]) -> Vec<String> {
    /// Merge options whose value rides in the FOLLOWING token.
    const VALUE_FLAGS: &[&str] = &[
        "-m",
        "--message",
        "-F",
        "--file",
        "-s",
        "--strategy",
        "-X",
        "--strategy-option",
        "--cleanup",
        "--into-name",
    ];
    let mut sources = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let a = rest[i].as_str();
        if a == "--" {
            sources.extend(rest[i + 1..].iter().cloned());
            break;
        }
        if a.starts_with('-') {
            i += if VALUE_FLAGS.contains(&a) { 2 } else { 1 };
            continue;
        }
        sources.push(a.to_string());
        i += 1;
    }
    sources
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rules_with_build(build: &[&str]) -> ResolvedCommands {
        ResolvedCommands {
            allow: ["git", "chmod", "cat", "diff"]
                .into_iter()
                .map(String::from)
                .collect(),
            default_build: build.iter().map(|s| (*s).to_string()).collect(),
            ..ResolvedCommands::default()
        }
    }

    #[test]
    fn read_tier_for_reads_and_read_only_git() {
        let rules = rules_with_build(&["make"]);
        assert_eq!(
            classify_command("cat src/main.rs", &[], &rules, None),
            Tier::Read
        );
        assert_eq!(
            classify_command("git log --oneline", &[], &rules, None),
            Tier::Read
        );
        assert_eq!(
            classify_command("git status", &[], &rules, None),
            Tier::Read
        );
        assert_eq!(
            classify_command("git diff HEAD~1", &[], &rules, None),
            Tier::Read
        );
    }

    #[test]
    fn edit_tier_when_writes_resolved() {
        let rules = rules_with_build(&["make"]);
        // A resolved write with no stateful position is Edit.
        let writes = vec![PathBuf::from("/repo/src/main.rs")];
        assert_eq!(
            classify_command("cp a.rs src/main.rs", &writes, &rules, None),
            Tier::Edit
        );
    }

    #[test]
    fn stateful_tier_for_mutating_git_subcommands() {
        let rules = rules_with_build(&["make"]);
        for cmd in [
            "git commit -m msg",
            "git checkout main",
            "git stash",
            "git stash pop",
            "git merge feature",
            "git rebase main",
        ] {
            assert_eq!(
                classify_command(cmd, &[], &rules, None),
                Tier::Stateful,
                "{cmd} must be stateful"
            );
        }
    }

    #[test]
    fn stateful_tier_survives_git_global_options() {
        let rules = rules_with_build(&["make"]);
        // A value-carrying global (`-c key=val`) is stepped over, so the mutating
        // subcommand is still seen.
        assert_eq!(
            classify_command("git -c core.editor=true commit -m x", &[], &rules, None),
            Tier::Stateful
        );
    }

    #[test]
    fn reconcile_direction_survives_git_global_options() {
        // `git -c key=val stash pop` still reads as a pop (book), not confused by
        // the config value operand.
        assert_eq!(
            git_reconcile_action("git -c gc.auto=0 stash pop"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Book))
        );
        assert_eq!(
            git_reconcile_action("git -c gc.auto=0 stash"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Unbook))
        );
    }

    #[test]
    fn stateful_tier_for_build_command() {
        let rules = rules_with_build(&["make"]);
        assert_eq!(
            classify_command("make check", &[], &rules, None),
            Tier::Stateful
        );
    }

    #[test]
    fn stateful_tier_for_chmod() {
        let rules = rules_with_build(&["make"]);
        assert_eq!(
            classify_command("chmod +x script.sh", &[], &rules, None),
            Tier::Stateful
        );
    }

    #[test]
    fn stateful_outranks_edit() {
        let rules = rules_with_build(&["make"]);
        // Even with a resolved write, a stateful position wins.
        let writes = vec![PathBuf::from("/repo/x")];
        assert_eq!(
            classify_command("git checkout -- src/main.rs", &writes, &rules, None),
            Tier::Stateful
        );
    }

    #[test]
    fn stateful_lifts_a_whole_chain() {
        let rules = rules_with_build(&["make"]);
        // A stateful command anywhere in a chain lifts the line.
        assert_eq!(
            classify_command("cat notes && git commit -m x", &[], &rules, None),
            Tier::Stateful
        );
    }

    #[test]
    fn read_tier_requires_no_lock_stateful_and_edit_do() {
        assert!(!Tier::Read.requires_lock());
        assert!(Tier::Edit.requires_lock());
        assert!(Tier::Stateful.requires_lock());
    }

    #[test]
    fn reconcile_direction_unbook_for_stash_and_checkout() {
        assert_eq!(
            git_reconcile_action("git stash"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Unbook))
        );
        assert_eq!(
            git_reconcile_action("git stash push"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Unbook))
        );
        assert_eq!(
            git_reconcile_action("git checkout -- src/main.rs"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Unbook))
        );
        assert_eq!(
            git_reconcile_action("git checkout main"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Unbook))
        );
    }

    #[test]
    fn reconcile_direction_book_for_pop_and_rebase() {
        assert_eq!(
            git_reconcile_action("git stash pop"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Book))
        );
        assert_eq!(
            git_reconcile_action("git stash apply"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Book))
        );
        assert_eq!(
            git_reconcile_action("git rebase main"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Book))
        );
    }

    #[test]
    fn reconcile_direction_none_for_commit_and_non_git() {
        assert_eq!(git_reconcile_action("git commit -m x"), None);
        assert_eq!(git_reconcile_action("git status"), None);
        // `git pull` drives no reconcile at all — the pull-parity floor the
        // wf-01 merge narrowing was ruled against.
        assert_eq!(git_reconcile_action("git pull"), None);
        assert_eq!(git_reconcile_action("make check"), None);
        assert_eq!(git_reconcile_action("cat file"), None);
    }

    // ── The merge arm: transfer-only, never Book (wf-01) ─────────────────────

    #[test]
    fn merge_classifies_as_transfer_with_its_source_refs() {
        // The primary real-world shape: a lead squash-merging an agent
        // worktree's branch.
        assert_eq!(
            git_reconcile_action("git merge --squash agents/s1/w1"),
            Some(ReconcileAction::MergeTransfer {
                sources: vec!["agents/s1/w1".to_string()],
            })
        );
        // A full merge carries the same transfer semantics.
        assert_eq!(
            git_reconcile_action("git merge feature"),
            Some(ReconcileAction::MergeTransfer {
                sources: vec!["feature".to_string()],
            })
        );
        // Value-carrying options never masquerade as the source ref.
        assert_eq!(
            git_reconcile_action("git merge --no-ff -m \"landing\" topic"),
            Some(ReconcileAction::MergeTransfer {
                sources: vec!["topic".to_string()],
            })
        );
        // Operands after a literal `--` are refs.
        assert_eq!(
            git_reconcile_action("git merge --squash -- topic"),
            Some(ReconcileAction::MergeTransfer {
                sources: vec!["topic".to_string()],
            })
        );
    }

    #[test]
    fn merge_abort_reconciles_as_unbook_and_continue_transfers_nothing() {
        // `--abort` restores the pre-merge tree: entries the transfer booked go
        // clean and unbook — the cheap least-surprising abort behavior.
        assert_eq!(
            git_reconcile_action("git merge --abort"),
            Some(ReconcileAction::Ledger(ReconcileDirection::Unbook))
        );
        // `--continue` completes a conflicted merge (commits it). It names no
        // source, so it transfers nothing — and it must NOT unbook: the
        // just-committed transferred debt stays due.
        assert_eq!(
            git_reconcile_action("git merge --continue"),
            Some(ReconcileAction::MergeTransfer { sources: vec![] })
        );
    }
}
