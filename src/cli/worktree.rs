// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! `catenary worktree` — the sanctioned worktree lifecycle surface (misc 151).
//!
//! Three verbs, the agent-facing replacement for raw `git worktree` (which the
//! command filter denies):
//!
//! - `ls` — the registry+sidecar view (Search-class, pipe-friendly): path, class,
//!   creator, age, clean/dirty, root state, and — for feats — ahead/behind
//!   upstream. Served by the daemon (`tool/worktree-ls`), which merges the
//!   durable sidecars with live mount state.
//! - `add <branch> [path]` — create a durable *feats*-class worktree with a
//!   sibling symlink. Filesystem-local (no daemon needed); runs the same
//!   `git worktree add` machinery as the creation hook.
//! - `rm <path>` — one removal verb, class-appropriate: an agent worktree removes
//!   on the caller's captured-work assertion (the force-shaped landing path); a
//!   feats worktree refuses dirty (uncommitted or unpushed). Served by the daemon
//!   (`tool/worktree-rm`), which reaps the mount, disposes, and firehose-logs.
//!
//! The former `diff` and `land` verbs retired to teaching stubs (wf-03, closing
//! bugs 115/119): landing is git-native now — commit in the branch, review with
//! `git diff main...<branch>`, `git merge --squash <branch>`, commit, then
//! `catenary worktree rm <path>`. The stubs ([`retired_stub_message`]) print
//! that flow and exit 2; they get deleted in a later release.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::Output;

/// `catenary worktree ls` — print the registry+sidecar view.
///
/// # Errors
///
/// Returns an error if no daemon is running or the response is invalid.
pub async fn run_ls(out: &mut Output) -> Result<()> {
    use std::fmt::Write as _;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let ipc_path = crate::router::socket_path();
    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({ "method": "tool/worktree-ls" });
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let response: serde_json::Value =
        serde_json::from_str(line.trim()).context("invalid response from daemon")?;
    let rows = response
        .get("worktrees")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    if rows.is_empty() {
        let _ = out.writeln(format_args!("No Catenary-managed worktrees"));
        return Ok(());
    }

    for row in &rows {
        let path = row.get("path").and_then(|v| v.as_str()).unwrap_or("?");
        let class = row.get("class").and_then(|v| v.as_str()).unwrap_or("?");
        let creator = row.get("creator").and_then(|v| v.as_str()).unwrap_or("?");
        let dirty = row
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let root_state = row
            .get("root_state")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let age = row
            .get("created_at")
            .and_then(|v| v.as_str())
            .map_or_else(|| "?".to_string(), humanize_age);
        let clean = if dirty { "dirty" } else { "clean" };

        let mut meta = format!("{class} · {creator} · {age} · {clean} · {root_state}");
        if let (Some(ahead), Some(behind)) = (
            row.get("ahead").and_then(serde_json::Value::as_u64),
            row.get("behind").and_then(serde_json::Value::as_u64),
        ) {
            let _ = write!(meta, " · ahead {ahead}, behind {behind}");
        }

        let _ = out.writeln(format_args!(
            "{path}  {}",
            out.colors.dim(&format!("[{meta}]"))
        ));
    }
    Ok(())
}

/// `catenary worktree add <branch> [path]` — create a durable feats worktree.
///
/// # Errors
///
/// Returns an error if the cwd is not a git repo, a collision is detected, the
/// symlink guard fails, or `git worktree add` fails.
pub fn run_add(out: &mut Output, branch: &str, path: Option<&Path>) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot resolve the current directory")?;
    let meta = crate::worktree_create::create_feat_worktree(&cwd, branch, path)?;
    let _ = out.writeln(format_args!(
        "created durable worktree: {}",
        meta.worktree.display()
    ));
    if let Some(link) = &meta.link {
        let _ = out.writeln(format_args!("  symlink: {}", link.display()));
    }
    Ok(())
}

/// `catenary worktree rm <path> [--force]` — remove a worktree
/// (class-appropriate).
///
/// Without `--force`, a dirty worktree is refused exactly as before (dirty
/// worktrees are never auto-cleaned). `--force` is the explicit, user-typed
/// exception: it discards a dirty worktree through the proper disposal path
/// (retire the root, sweep the sidecar) and the output names what was
/// discarded — the dirty-file summary the daemon returns.
///
/// # Errors
///
/// Returns an error if no daemon is running or the response is invalid. A refusal
/// (dirty feats worktree, git refusal, non-Catenary path) is printed, not an
/// error — the caller still exits successfully.
pub async fn run_rm(out: &mut Output, path: PathBuf, force: bool) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    // Absolutize before sending: the daemon's cwd differs from ours, so a
    // relative path must be resolved here.
    let abs = path.canonicalize().unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|c| c.join(&path))
            .unwrap_or(path)
    });

    let ipc_path = crate::router::socket_path();
    let stream = tokio::net::UnixStream::connect(&ipc_path)
        .await
        .context("no daemon running — start a Catenary session first")?;

    let (reader, mut writer) = stream.into_split();
    let request = serde_json::json!({
        "method": "tool/worktree-rm",
        "path": abs.display().to_string(),
        "force": force,
    });
    let mut payload = serde_json::to_string(&request)?;
    payload.push('\n');
    writer.write_all(payload.as_bytes()).await?;

    let mut buf_reader = BufReader::new(reader);
    let mut line = String::new();
    buf_reader.read_line(&mut line).await?;

    let response: serde_json::Value =
        serde_json::from_str(line.trim()).context("invalid response from daemon")?;
    let status = response
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    match status {
        "ok" => {
            // A forced discard names what it dropped (the dirty summary); a clean
            // removal just confirms the path.
            if let Some(discarded) = response.get("discarded").and_then(|v| v.as_str()) {
                let _ = out.writeln(format_args!(
                    "discarded dirty worktree ({discarded}): {}",
                    abs.display()
                ));
            } else {
                let _ = out.writeln(format_args!("removed worktree: {}", abs.display()));
            }
        }
        "kept" | "refused" | "not_ours" | "error" => {
            let msg = response
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("worktree not removed");
            let _ = out.writeln(format_args!("{msg}"));
        }
        _ => anyhow::bail!("unexpected response from daemon"),
    }
    Ok(())
}

/// The teaching message the retired `worktree diff` / `worktree land` stubs
/// print (wf-03, closing bugs 115/119).
///
/// The patch engine (`git apply` reimplementing `git merge`) is gone; landing is
/// git-native and the merge bracket (wf-01) carries the debt transfer. The
/// message names the verb invoked and teaches the exact sequence, one step per
/// line. The stubs exit 2 (distinct from success and from generic error 1).
///
/// NOTE: these stubs are transition-period teaching only — they (and this
/// function) get deleted in a later release.
#[must_use]
pub fn retired_stub_message(verb: &str) -> String {
    format!(
        "`catenary worktree {verb}` is retired — land worktree work with git itself:\n\
         \x20 1. commit the work in the worktree, on its branch\n\
         \x20 2. review it: git diff main...<branch>\n\
         \x20 3. in the owning repo: git merge --squash <branch>\n\
         \x20 4. commit the result\n\
         \x20 5. remove the worktree: catenary worktree rm <path>\n\
         The merge bracket transfers any unpaid worker debt automatically; pay it \
         with `catenary diagnostics`.\n"
    )
}

/// Humanize an RFC 3339 timestamp into a compact relative age (`3h`, `2d`, `5m`,
/// `just now`). Falls back to the raw string when it cannot be parsed.
fn humanize_age(created_at: &str) -> String {
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(created_at) else {
        return created_at.to_string();
    };
    let secs = (chrono::Utc::now() - created.with_timezone(&chrono::Utc)).num_seconds();
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::{humanize_age, retired_stub_message};

    #[test]
    fn retired_stub_names_the_verb_and_teaches_the_git_native_flow() {
        // The transition stubs (wf-03) must name the invoked verb and carry the
        // exact landing sequence, one step per line, plus the debt note. (The
        // stubs delete in a later release — this test goes with them.)
        for verb in ["diff", "land"] {
            let msg = retired_stub_message(verb);
            assert!(
                msg.starts_with(&format!("`catenary worktree {verb}` is retired")),
                "stub names the invoked verb: {msg}"
            );
            for step in [
                "commit the work in the worktree",
                "git diff main...<branch>",
                "git merge --squash <branch>",
                "commit the result",
                "catenary worktree rm <path>",
                "merge bracket transfers any unpaid worker debt automatically",
                "`catenary diagnostics`",
            ] {
                assert!(msg.contains(step), "stub missing {step:?}: {msg}");
            }
        }
    }

    #[test]
    fn humanize_age_buckets() {
        let now = chrono::Utc::now();
        assert_eq!(humanize_age(&now.to_rfc3339()), "just now");
        assert_eq!(
            humanize_age(&(now - chrono::Duration::minutes(5)).to_rfc3339()),
            "5m",
        );
        assert_eq!(
            humanize_age(&(now - chrono::Duration::hours(3)).to_rfc3339()),
            "3h",
        );
        assert_eq!(
            humanize_age(&(now - chrono::Duration::days(2)).to_rfc3339()),
            "2d",
        );
    }

    #[test]
    fn humanize_age_unparseable_is_verbatim() {
        assert_eq!(humanize_age("not-a-date"), "not-a-date");
    }
}
