// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Canonical `clap::Command` definitions for agent-facing CLI commands.
//!
//! Single source of truth for help text. Consumed by:
//! - The binary (`main.rs`) to override derive-generated help via
//!   `mut_subcommand`.
//! - `render_subcommand_help` in `command_filter.rs` for redirect
//!   denial messages.
//! - `run_primer` in `main.rs` via `primer_commands()`.

use clap::{Arg, ArgAction, Command};

/// Build the canonical `catenary grep` command definition.
///
/// Arg IDs must match the derive field names in `main.rs` so
/// `FromArgMatches` can extract values after `mut_subcommand`.
#[must_use]
pub fn grep_command() -> Command {
    Command::new("grep")
        .about("Search for a pattern with LSP-enriched results")
        .after_help(
            "Searches from the current working directory. Results within tracked\n\
             workspace roots include symbol context from LSP servers.",
        )
        .arg(
            Arg::new("pattern")
                .required(true)
                .help("Regex pattern (Rust/PCRE syntax, | for alternation)"),
        )
        .arg(Arg::new("GLOB").help(
            "Scope the search (e.g., src/**/*.rs, **/*.{ts,js},\n\
                 /home/user/project/**/*.py)",
        ))
        .arg(
            Arg::new("exclude")
                .long("exclude")
                .help("Exclude matches (e.g., tests/**)"),
        )
        .arg(
            Arg::new("page")
                .long("page")
                .default_value("1")
                .value_parser(clap::value_parser!(usize))
                .help("Page number for paged results"),
        )
        .arg(
            Arg::new("include_gitignored")
                .long("include-gitignored")
                .action(ArgAction::SetTrue)
                .help("Include files ignored by .gitignore"),
        )
        .arg(
            Arg::new("include_hidden")
                .long("include-hidden")
                .action(ArgAction::SetTrue)
                .help("Include hidden files and directories"),
        )
}

/// Build the canonical `catenary glob` command definition.
///
/// Arg IDs must match the derive field names in `main.rs` so
/// `FromArgMatches` can extract values after `mut_subcommand`.
#[must_use]
pub fn glob_command() -> Command {
    Command::new("glob")
        .about("Browse the filesystem: file outlines, directory listings, glob patterns")
        .after_help(
            "Resolves against the current working directory. Results include symbol\n\
             outlines when LSP data is available.",
        )
        .arg(Arg::new("pattern").required(true).help(
            "File, directory, or glob (e.g., src/, **/*.{rs,toml},\n\
                     /home/user/project/src/)",
        ))
        .arg(
            Arg::new("exclude")
                .long("exclude")
                .help("Exclude matches (e.g., tests/**)"),
        )
        .arg(
            Arg::new("page")
                .long("page")
                .default_value("1")
                .value_parser(clap::value_parser!(usize))
                .help("Page number for paged results"),
        )
        .arg(
            Arg::new("include_gitignored")
                .long("include-gitignored")
                .action(ArgAction::SetTrue)
                .help("Include files ignored by .gitignore"),
        )
        .arg(
            Arg::new("include_hidden")
                .long("include-hidden")
                .action(ArgAction::SetTrue)
                .help("Include hidden files and directories"),
        )
}

/// Build the canonical `catenary editing` command definition.
#[must_use]
pub fn editing_command() -> Command {
    Command::new("editing")
        .about("Editing mode (start, stop)")
        .subcommand(Command::new("start").about("Enter editing mode"))
        .subcommand(Command::new("stop").about("Exit editing mode and print diagnostics"))
        .subcommand_required(true)
}

/// Build the canonical `catenary roots` command definition.
#[must_use]
pub fn roots_command() -> Command {
    Command::new("roots")
        .about("Workspace root management (add, rm, ls)")
        .subcommand(
            Command::new("add").about("Add a workspace root").arg(
                Arg::new("path")
                    .required(true)
                    .help("Path to add as a workspace root"),
            ),
        )
        .subcommand(
            Command::new("rm").about("Remove a workspace root").arg(
                Arg::new("path")
                    .required(true)
                    .help("Path to remove from workspace roots"),
            ),
        )
        .subcommand(Command::new("ls").about("List all tracked workspace roots with their source"))
        .subcommand_required(true)
}

/// All agent-facing commands, for use by `catenary primer`.
///
/// Returns the commands in display order: editing, grep, glob, roots.
#[must_use]
pub fn primer_commands() -> Vec<Command> {
    vec![
        editing_command(),
        grep_command(),
        glob_command(),
        roots_command(),
    ]
}

/// Render the `-h` output for a Catenary subcommand.
///
/// Uses the canonical command definitions from this module. Returns an
/// empty string if the subcommand name is not recognized.
#[must_use]
pub fn render_help(subcommand: &str) -> String {
    let mut cmd = match subcommand {
        "grep" => grep_command(),
        "glob" => glob_command(),
        "editing" => editing_command(),
        "roots" => roots_command(),
        _ => return String::new(),
    };
    cmd = cmd.bin_name(format!("catenary {subcommand}"));
    cmd.render_help().to_string()
}
