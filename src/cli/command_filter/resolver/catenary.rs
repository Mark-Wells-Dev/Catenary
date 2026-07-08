// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Write-set resolution for `catenary worktree land` (misc 158).
//!
//! `catenary worktree land <path>` writes the worktree's complete diff into the
//! owning repo — a Write in `catenary`-subcommand clothing, exactly like
//! `git apply` is a Write in git clothing (decision 026 §2). So it must stay
//! inside the writes-resolve-or-deny law: the PreToolUse hook resolves land's
//! write set *before* execution and records it into the diagnostics batch, so
//! landing through the verb arms the gate precisely like today's manual
//! `git apply` path.
//!
//! The write set is exactly `worktree diff --name-only` mapped onto the owning
//! repo: read the worktree's sidecar for its `source_repo`, ask git for the
//! changed-path list ([`crate::worktree_land::worktree_changed_paths`]), and
//! join each onto the owning repo root. A computed worktree argument (a `$VAR` /
//! `$(…)` / unquoted glob), a missing sidecar, or a git-query failure is Opaque
//! — the same fail-closed direction as `git apply`.
//!
//! Every other `catenary` subcommand introduces no *attributable* uncommitted
//! content into the working tree (they run under the canonical-form matcher,
//! regime 1), so they keep the inherited `NoWrite` boundary.

use std::collections::BTreeSet;
use std::path::PathBuf;

use super::{SegmentClass, State, Unresolved, run, u};
use crate::cli::command_filter::parse::{SimpleCommand, WordMeta};

/// Resolve a `catenary` invocation's write-set: `worktree land <path>` resolves
/// to the worktree's changed paths mapped onto the owning repo; everything else
/// catenary is `NoWrite`.
pub(super) fn resolve_catenary(cmd: &SimpleCommand, state: &State, seg_name: &str) -> SegmentClass {
    // The write verb is `worktree land <path>`. Any other catenary form (search,
    // lifecycle, `worktree diff/ls/add/rm`) introduces no attributable working-
    // tree content here.
    match (cmd.argv.first(), cmd.argv.get(1)) {
        (Some(a), Some(b)) if a == "worktree" && b == "land" => {
            run(resolve_land(&cmd.argv, &cmd.argv_meta, state), seg_name)
        }
        _ => SegmentClass::NoWrite,
    }
}

/// Resolve `catenary worktree land <path>` → the worktree's changed paths mapped
/// onto the owning repo (from the sidecar's `source_repo`).
///
/// The path operand (the first non-flag word after `land`) must be literal; a
/// computed path, a missing sidecar, or a git-query failure is Opaque.
fn resolve_land(
    argv: &[String],
    metas: &[WordMeta],
    state: &State,
) -> Result<SegmentClass, Unresolved> {
    // The worktree path is the first non-flag operand after `worktree land`.
    let mut path_idx: Option<usize> = None;
    for (i, word) in argv.iter().enumerate().skip(2) {
        if is_flag(word) {
            continue;
        }
        path_idx = Some(i);
        break;
    }
    let Some(idx) = path_idx else {
        return Err(missing_path());
    };
    if !is_literal(metas.get(idx).copied().unwrap_or_default()) {
        return Err(opaque_path());
    }

    // The worktree path resolves against the threaded cwd (the CLI absolutizes it
    // the same way). A relative path under a poisoned/relative cwd is Opaque.
    let worktree = super::resolve_path(&argv[idx], state).map_err(|_| opaque_path())?;

    // The owning repo comes from the worktree's sidecar (the registered meta).
    let sidecar = crate::worktree_create::sidecar_path(&worktree);
    let meta = std::fs::read_to_string(&sidecar)
        .ok()
        .and_then(|c| serde_json::from_str::<crate::worktree_create::WorktreeMeta>(&c).ok())
        .ok_or_else(not_registered)?;

    // The changed paths are worktree-relative (git's diff convention); map each
    // onto the owning repo root — the exact set the land will write there.
    let rels =
        crate::worktree_land::worktree_changed_paths(&worktree).map_err(|_| query_failed())?;
    let mut writes: BTreeSet<PathBuf> = BTreeSet::new();
    for rel in &rels {
        writes.insert(super::normalize(&meta.source_repo.join(rel)));
    }
    Ok(SegmentClass::Recorded(writes))
}

/// Whether an argv word is an option (`-x`, `--long`) rather than an operand.
fn is_flag(a: &str) -> bool {
    a.len() > 1 && a.starts_with('-')
}

/// Whether a word is a plain literal usable in a path query (no substitution or
/// live expansion). A quoted-metacharacter word is literal.
const fn is_literal(meta: WordMeta) -> bool {
    !meta.value_subs && !meta.process_subs && !meta.any_live()
}

// ── teaching denials ─────────────────────────────────────────────────────────

fn missing_path() -> Unresolved {
    u(
        "worktree-land-no-path",
        "`catenary worktree land` needs the worktree path to resolve which files it \
         would write into the owning repo. Name the worktree path.",
    )
}

fn opaque_path() -> Unresolved {
    u(
        "worktree-land-opaque-path",
        "The worktree path for `catenary worktree land` is assembled at runtime (`$VAR` / \
         `$(…)` / an unquoted glob), so the hook can't read which files it would land. \
         Name the worktree path literally.",
    )
}

fn not_registered() -> Unresolved {
    u(
        "worktree-land-not-registered",
        "That path is not a registered Catenary worktree (no sidecar), so the hook can't \
         find the owning repo to resolve the land's write-set. Check the path — \
         `catenary worktree ls` lists the managed worktrees.",
    )
}

fn query_failed() -> Unresolved {
    u(
        "worktree-land-query-failed",
        "git couldn't tell the hook which files this worktree would land — it isn't a git \
         worktree, or it has been removed. Check the path with `catenary worktree ls`.",
    )
}
