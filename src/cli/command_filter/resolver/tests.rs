// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

//! Per-form unit coverage for the write resolver (ws38 ticket 01): one test
//! cluster per registry form, plus the composition table.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tempfile::TempDir;

use super::{
    OpaqueWrite, Position, SegCtx, SegmentClass, State, expand_word, resolve_command,
    resolve_segment,
};
use crate::cli::command_filter::parse;

/// Resolve expecting success; returns the recorded write-set.
fn ok(cmd: &str, cwd: &Path) -> BTreeSet<PathBuf> {
    resolve_command(cmd, Some(cwd))
        .unwrap_or_else(|op| panic!("expected {cmd:?} to resolve, got {op:?}"))
        .writes
}

/// Resolve expecting an opaque write; returns it.
fn err(cmd: &str, cwd: Option<&Path>) -> OpaqueWrite {
    match resolve_command(cmd, cwd) {
        Ok(lw) => panic!("expected {cmd:?} to be opaque, resolved to {lw:?}"),
        Err(op) => op,
    }
}

/// Shorthand: the expected write-set as cwd-joined paths.
fn paths(cwd: &Path, rel: &[&str]) -> BTreeSet<PathBuf> {
    rel.iter().map(|r| cwd.join(r)).collect()
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn touch(dir: &Path, rel: &str) {
    let p = dir.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(&p, b"x").expect("touch");
}

/// Classify the first segment of a command directly.
fn classify(cmd: &str, cwd: Option<&Path>) -> SegmentClass {
    let script = parse::parse(cmd);
    let pipeline = &script.pipelines[0];
    let mut state = State::new(cwd);
    resolve_segment(
        &pipeline.commands[0],
        &mut state,
        SegCtx {
            conditional: false,
            sole_stage: pipeline.commands.len() == 1,
        },
    )
}

// ── Redirects (shell grammar, soundness layer 1) ─────────────────────────────

#[test]
fn redirect_literal_target_recorded() {
    let t = tmp();
    assert_eq!(
        ok("echo hi > out.txt", t.path()),
        paths(t.path(), &["out.txt"])
    );
    assert_eq!(
        ok("git log >> notes.md", t.path()),
        paths(t.path(), &["notes.md"])
    );
    assert_eq!(
        ok("echo x >| clob.txt", t.path()),
        paths(t.path(), &["clob.txt"])
    );
    assert_eq!(
        ok("make test &> build.log", t.path()),
        paths(t.path(), &["build.log"])
    );
    assert_eq!(
        ok("make test 2>err.log", t.path()),
        paths(t.path(), &["err.log"])
    );
}

#[test]
fn redirect_absolute_target_recorded() {
    let t = tmp();
    let target = t.path().join("abs.txt");
    let cmd = format!("echo hi > {}", target.display());
    assert_eq!(ok(&cmd, Path::new("/elsewhere")), BTreeSet::from([target]));
}

#[test]
fn sinks_are_not_writes() {
    let t = tmp();
    for cmd in [
        "make test > /dev/null",
        "make test > /dev/stdout",
        "make test > /dev/stderr",
        "make test 2>&1",
        "make test >&2",
        "make test >&-",
        "make test > /dev/null 2>&1",
    ] {
        assert_eq!(ok(cmd, t.path()), BTreeSet::new(), "{cmd}");
    }
}

#[test]
fn dup_out_file_target_recorded() {
    let t = tmp();
    assert_eq!(
        ok("make test >&out.log", t.path()),
        paths(t.path(), &["out.log"])
    );
}

#[test]
fn multios_targets_all_recorded() {
    let t = tmp();
    assert_eq!(
        ok("echo hi > a > b", t.path()),
        paths(t.path(), &["a", "b"])
    );
}

#[test]
fn heredoc_redirect_target_resolves() {
    let t = tmp();
    assert_eq!(
        ok("cat > f.rs <<'EOF'\nfn main() {}\nEOF", t.path()),
        paths(t.path(), &["f.rs"]),
    );
}

#[test]
fn unquoted_heredoc_body_substitution_writes_resolve() {
    let t = tmp();
    touch(t.path(), "a");
    // The body's `$(cp a b)` runs (bug 46): its landing joins the union.
    assert_eq!(
        ok("cat > f <<EOF\n$(cp a b)\nEOF", t.path()),
        paths(t.path(), &["f", "b"]),
    );
}

#[test]
fn dangling_redirect_is_opaque() {
    assert_eq!(err("echo hi >", None).construct, "dangling-redirect");
}

#[test]
fn subshell_trailing_redirect_recorded() {
    let t = tmp();
    assert_eq!(ok("( echo hi ) > f", t.path()), paths(t.path(), &["f"]));
}

#[test]
fn backgrounded_write_recorded() {
    let t = tmp();
    assert_eq!(ok("echo x > f &", t.path()), paths(t.path(), &["f"]));
}

// ── Variables: bound / unbound / tainted (statically expandable forms) ───────

#[test]
fn unbound_variable_target_is_opaque() {
    let op = err("echo x > $FILENAME", None);
    assert_eq!(op.construct, "unbound-variable");
    assert!(op.message.contains("FILENAME"), "{}", op.message);
    assert!(op.message.contains("Bind it"), "teaching: {}", op.message);
}

#[test]
fn same_line_binding_resolves() {
    let t = tmp();
    let want = paths(t.path(), &["out.txt"]);
    assert_eq!(ok("F=out.txt; echo x > $F", t.path()), want);
    assert_eq!(ok("F=out.txt; echo x > \"$F\"", t.path()), want);
    assert_eq!(ok("F=out.txt; echo x > ${F}", t.path()), want);
    assert_eq!(ok("F=out.txt && echo x > $F", t.path()), want);
}

#[test]
fn conditional_binding_taints() {
    let op = err("true && F=a.txt; echo x > $F", None);
    assert_eq!(op.construct, "tainted-variable");
}

#[test]
fn var_mutator_taints_downstream_targets() {
    assert_eq!(
        err("read F; echo x > $F", None).construct,
        "runtime-variables"
    );
    assert_eq!(
        err("F=a; export G=b; echo x > $F", None).construct,
        "runtime-variables",
    );
}

#[test]
fn parameter_operator_forms_are_opaque() {
    assert_eq!(
        err("echo x > ${F:-fallback}", None).construct,
        "parameter-expansion-form",
    );
    assert_eq!(
        err("echo x > ${!F}", None).construct,
        "parameter-expansion-form"
    );
    assert_eq!(err("echo x > $1", None).construct, "special-parameter");
}

#[test]
fn quoted_dollar_is_a_literal_path() {
    let t = tmp();
    assert_eq!(ok("echo x > '$F'", t.path()), paths(t.path(), &["$F"]));
}

#[test]
fn mixed_quoting_is_opaque() {
    assert_eq!(err("echo x > '$A'$B", None).construct, "mixed-quoting");
}

#[test]
fn command_substitution_target_is_opaque() {
    assert_eq!(
        err("echo x > $(pick-name)", None).construct,
        "command-substitution-target",
    );
    assert_eq!(
        err("echo x > `pick-name`", None).construct,
        "command-substitution-target",
    );
}

#[test]
fn process_substitution_target_is_a_pipe_with_inner_writes() {
    let t = tmp();
    assert_eq!(
        ok("sort x > >(tee out.log)", t.path()),
        paths(t.path(), &["out.log"]),
    );
}

#[test]
fn variable_expanding_to_flag_is_opaque() {
    let t = tmp();
    touch(t.path(), "a");
    let cmd = "R=-r; cp $R a b";
    assert_eq!(
        resolve_command(cmd, Some(t.path()))
            .expect_err("flag from var")
            .construct,
        "computed-flag",
    );
}

// ── Tilde and brace expansion ────────────────────────────────────────────────

#[test]
fn tilde_target_resolves_to_home() {
    let t = tmp();
    let home = dirs::home_dir().expect("home dir");
    assert_eq!(
        ok("echo x > ~/cat-res-test.txt", t.path()),
        BTreeSet::from([home.join("cat-res-test.txt")]),
    );
}

#[test]
fn tilde_user_is_opaque() {
    assert_eq!(err("echo x > ~root/f", None).construct, "tilde-user");
}

#[test]
fn brace_expansion_resolves() {
    let t = tmp();
    assert_eq!(
        ok("echo x > {a,b}.txt", t.path()),
        paths(t.path(), &["a.txt", "b.txt"]),
    );
    assert_eq!(
        ok("echo x > f{1..3}", t.path()),
        paths(t.path(), &["f1", "f2", "f3"]),
    );
}

#[test]
fn invalid_brace_group_stays_literal() {
    // Bash keeps `{a}` literal — so does the resolver.
    let t = tmp();
    assert_eq!(
        ok("echo x > {a}.txt", t.path()),
        paths(t.path(), &["{a}.txt"])
    );
}

// ── Globs in write position (state query) ────────────────────────────────────

#[test]
fn glob_in_single_target_position_is_opaque() {
    let t = tmp();
    assert_eq!(
        resolve_command("echo x > *.txt", Some(t.path()))
            .expect_err("glob redirect")
            .construct,
        "glob-single-target",
    );
}

#[test]
fn glob_in_file_list_expands_against_filesystem() {
    let t = tmp();
    touch(t.path(), "a.rs");
    touch(t.path(), "b.rs");
    touch(t.path(), "c.txt");
    assert_eq!(
        ok("sed -i 's/x/y/' *.rs", t.path()),
        paths(t.path(), &["a.rs", "b.rs"]),
    );
}

#[test]
fn globstar_expands_recursively() {
    let t = tmp();
    touch(t.path(), "src/mod.rs");
    touch(t.path(), "src/deep/mod.rs");
    assert_eq!(
        ok("sed -i 's/a/b/' src/**/mod.rs", t.path()),
        paths(t.path(), &["src/mod.rs", "src/deep/mod.rs"]),
    );
}

#[test]
fn zero_match_glob_records_the_literal_word() {
    // nullglob off: the shell passes the pattern through, and a creator like
    // tee writes a file by that literal name.
    let t = tmp();
    assert_eq!(
        ok("echo x | tee out-*.log", t.path()),
        paths(t.path(), &["out-*.log"]),
    );
}

#[test]
fn relative_glob_without_cwd_is_opaque() {
    assert_eq!(
        err("sed -i 's/a/b/' *.rs", None).construct,
        "no-cwd-for-query",
    );
}

// ── cwd threading ────────────────────────────────────────────────────────────

#[test]
fn literal_cd_threads_relative_targets() {
    let t = tmp();
    assert_eq!(
        ok("cd sub && echo x > gen.rs", t.path()),
        paths(t.path(), &["sub/gen.rs"]),
    );
    assert_eq!(
        ok("cd sub; cd more; echo x > f", t.path()),
        paths(t.path(), &["sub/more/f"]),
    );
    assert_eq!(
        ok("cd sub && cd .. && echo x > f", t.path()),
        paths(t.path(), &["f"])
    );
}

#[test]
fn bound_variable_cd_threads() {
    let t = tmp();
    let cmd = format!("D={}; cd $D && echo x > f", t.path().display());
    assert_eq!(ok(&cmd, Path::new("/elsewhere")), paths(t.path(), &["f"]));
}

#[test]
fn opaque_cd_poisons_downstream_relative_targets() {
    let op = err("cd $DIR && echo x > f", None);
    assert_eq!(op.construct, "opaque-cwd");
    assert_eq!(err("cd - && echo x > f", None).construct, "opaque-cwd");
}

#[test]
fn opaque_cd_leaves_absolute_targets_resolvable() {
    let t = tmp();
    let target = t.path().join("abs.txt");
    let cmd = format!("cd $DIR && echo x > {}", target.display());
    assert_eq!(
        resolve_command(&cmd, Some(t.path()))
            .expect("absolute survives poison")
            .writes,
        BTreeSet::from([target]),
    );
}

#[test]
fn pipeline_stage_cd_does_not_escape_its_subshell() {
    let t = tmp();
    assert_eq!(
        ok("cd /elsewhere | cat; echo x > f", t.path()),
        paths(t.path(), &["f"]),
    );
}

#[test]
fn subshell_cd_poisons_conservatively() {
    // `( cd … )` scoping isn't modeled — the cwd fails toward poison, and
    // only a later relative target turns that into a denial.
    assert_eq!(err("(cd /tmp); echo x > f", None).construct, "opaque-cwd");
}

#[test]
fn no_cwd_records_relative_paths() {
    assert_eq!(
        resolve_command("echo x > f.txt", None)
            .expect("relative ok")
            .writes,
        BTreeSet::from([PathBuf::from("f.txt")]),
    );
}

// ── cp / mv / rm (argument-convention movers) ────────────────────────────────

#[test]
fn cp_to_file_records_destination() {
    let t = tmp();
    touch(t.path(), "src.txt");
    assert_eq!(
        ok("cp src.txt dst.txt", t.path()),
        paths(t.path(), &["dst.txt"])
    );
}

#[test]
fn cp_into_directory_records_landing() {
    let t = tmp();
    touch(t.path(), "src.txt");
    std::fs::create_dir(t.path().join("d")).expect("mkdir");
    assert_eq!(
        ok("cp src.txt d", t.path()),
        paths(t.path(), &["d/src.txt"])
    );
}

#[test]
fn cp_dev_stdin_is_a_conduit_to_the_destination() {
    let t = tmp();
    assert_eq!(
        ok("echo hi | cp /dev/stdin f.txt", t.path()),
        paths(t.path(), &["f.txt"]),
    );
}

#[test]
fn cp_recursive_enumerates_the_tree() {
    let t = tmp();
    touch(t.path(), "tree/a");
    touch(t.path(), "tree/sub/b");
    std::fs::create_dir(t.path().join("dstdir")).expect("mkdir");
    assert_eq!(
        ok("cp -r tree dstdir", t.path()),
        paths(
            t.path(),
            &["dstdir/tree", "dstdir/tree/a", "dstdir/tree/sub/b"]
        ),
    );
}

#[test]
fn cp_glob_sources_expand() {
    let t = tmp();
    touch(t.path(), "a1.rs");
    touch(t.path(), "a2.rs");
    std::fs::create_dir(t.path().join("d")).expect("mkdir");
    assert_eq!(
        ok("cp *.rs d", t.path()),
        paths(t.path(), &["d/a1.rs", "d/a2.rs"]),
    );
}

#[test]
fn cp_unmodeled_flag_is_opaque() {
    let t = tmp();
    let op = resolve_command("cp -t d src.txt", Some(t.path())).expect_err("-t unmodeled");
    assert_eq!(op.construct, "unmodeled-flag");
    assert!(op.message.contains("cp -t"), "{}", op.message);
}

#[test]
fn mv_records_the_destination_side() {
    let t = tmp();
    touch(t.path(), "a");
    assert_eq!(ok("mv a b", t.path()), paths(t.path(), &["b"]));
    std::fs::create_dir(t.path().join("d")).expect("mkdir");
    assert_eq!(ok("mv a d", t.path()), paths(t.path(), &["d/a"]));
}

#[test]
fn mv_directory_enumerates_landings() {
    let t = tmp();
    touch(t.path(), "tree/a");
    std::fs::create_dir(t.path().join("d")).expect("mkdir");
    assert_eq!(
        ok("mv tree d", t.path()),
        paths(t.path(), &["d/tree", "d/tree/a"]),
    );
}

// ── ln (the link path is the write) ──────────────────────────────────────────

#[test]
fn ln_symlink_records_the_link_name() {
    let t = tmp();
    touch(t.path(), "target.rs");
    // `ln -s TARGET LINK` → the link name is the write; the target is not.
    assert_eq!(
        ok("ln -s target.rs link.rs", t.path()),
        paths(t.path(), &["link.rs"]),
    );
    // Hard link, same shape.
    assert_eq!(
        ok("ln target.rs hard.rs", t.path()),
        paths(t.path(), &["hard.rs"])
    );
}

#[test]
fn ln_single_operand_links_basename_in_cwd() {
    let t = tmp();
    touch(t.path(), "src/mod.rs");
    // `ln TARGET` → link `basename(TARGET)` in the cwd.
    assert_eq!(
        ok("ln -s src/mod.rs", t.path()),
        paths(t.path(), &["mod.rs"])
    );
}

#[test]
fn ln_into_directory_records_the_landing() {
    let t = tmp();
    touch(t.path(), "a.rs");
    std::fs::create_dir(t.path().join("d")).expect("mkdir");
    // `ln TARGET DIR` (DIR exists) → the link lands at `DIR/basename(TARGET)`.
    assert_eq!(ok("ln -s a.rs d", t.path()), paths(t.path(), &["d/a.rs"]));
}

#[test]
fn ln_target_directory_flag_records_each_link() {
    let t = tmp();
    touch(t.path(), "a.rs");
    touch(t.path(), "b.rs");
    std::fs::create_dir(t.path().join("d")).expect("mkdir");
    // `ln -t DIR TARGET…` → every operand is a target; a link per target.
    assert_eq!(
        ok("ln -s -t d a.rs b.rs", t.path()),
        paths(t.path(), &["d/a.rs", "d/b.rs"]),
    );
    // Glued `-tDIR` form resolves the same.
    assert_eq!(ok("ln -s -td a.rs", t.path()), paths(t.path(), &["d/a.rs"]));
}

#[test]
fn ln_no_target_directory_flag_records_the_link() {
    let t = tmp();
    touch(t.path(), "a.rs");
    std::fs::create_dir(t.path().join("existing")).expect("mkdir");
    // `-T` forces the last operand to be the link name even when it names an
    // existing directory.
    assert_eq!(
        ok("ln -sT a.rs existing", t.path()),
        paths(t.path(), &["existing"]),
    );
    // `-T` with the wrong operand count is ambiguous → Opaque.
    assert_eq!(err("ln -T a", None).construct, "ln-ambiguous-link");
}

#[test]
fn ln_computed_link_name_is_opaque() {
    // A variable link name can't be pinned — Opaque, fail-closed.
    assert_eq!(
        err("ln -s target.rs $LINK", None).construct,
        "unbound-variable"
    );
}

#[test]
fn ln_unmodeled_flag_is_opaque() {
    assert_eq!(
        err("ln --backup=numbered -S .orig a b", None).construct,
        "unmodeled-flag"
    );
}

#[test]
fn ln_under_xargs_is_stdin_driven() {
    assert_eq!(
        err("echo a | xargs ln -s target", None).construct,
        "stdin-driven-targets",
    );
}

#[test]
fn rm_is_a_pure_delete() {
    assert_eq!(classify("rm -rf build/", None), SegmentClass::PureDelete);
    // Deletes carry no debt at line level.
    assert_eq!(
        resolve_command("rm -rf a b c", None).expect("rm ok").writes,
        BTreeSet::new()
    );
    // A redirect on the same segment is still a recorded write.
    let t = tmp();
    assert_eq!(
        ok("rm -v a > log.txt", t.path()),
        paths(t.path(), &["log.txt"])
    );
}

// ── tee ──────────────────────────────────────────────────────────────────────

#[test]
fn tee_records_file_operands() {
    let t = tmp();
    assert_eq!(
        ok("make 2>&1 | tee build.log other.log", t.path()),
        paths(t.path(), &["build.log", "other.log"]),
    );
    assert_eq!(
        ok("echo x | tee -a append.log", t.path()),
        paths(t.path(), &["append.log"])
    );
}

// ── sed (script checked: pure editing subset) ────────────────────────────────

#[test]
fn sed_in_place_records_files_and_backups() {
    let t = tmp();
    touch(t.path(), "a.rs");
    touch(t.path(), "b.rs");
    assert_eq!(
        ok("sed -i 's/x/y/' a.rs b.rs", t.path()),
        paths(t.path(), &["a.rs", "b.rs"]),
    );
    assert_eq!(
        ok("sed -i.bak 's/x/y/' a.rs", t.path()),
        paths(t.path(), &["a.rs", "a.rs.bak"]),
    );
    assert_eq!(
        ok("sed --in-place=.orig 's/x/y/' a.rs", t.path()),
        paths(t.path(), &["a.rs", "a.rs.orig"]),
    );
}

#[test]
fn sed_preview_is_no_write() {
    let t = tmp();
    touch(t.path(), "a.rs");
    assert_eq!(ok("sed 's/x/y/' a.rs", t.path()), BTreeSet::new());
    assert_eq!(ok("sed -n '1,10p' a.rs", t.path()), BTreeSet::new());
}

#[test]
fn sed_pure_editing_subset_passes() {
    let t = tmp();
    touch(t.path(), "f");
    assert_eq!(
        ok("sed -i -e '1,10d' -e 's/a\\/b/c/g; y/x/z/' f", t.path()),
        paths(t.path(), &["f"]),
    );
    assert_eq!(
        ok("sed -i '/start/,/end/{s/a/b/;d}' f", t.path()),
        paths(t.path(), &["f"]),
    );
}

#[test]
fn sed_write_commands_are_surgically_denied() {
    let t = tmp();
    touch(t.path(), "f");
    let op = resolve_command("sed -i '/x/w out.txt' f", Some(t.path())).expect_err("w denied");
    assert_eq!(op.construct, "sed-write-command");
    assert!(op.message.contains('w'), "{}", op.message);
    // The script check applies without -i too: `w` writes from a preview.
    assert_eq!(
        resolve_command("sed 'w hijack' f", Some(t.path()))
            .expect_err("preview w")
            .construct,
        "sed-write-command",
    );
    assert_eq!(
        resolve_command("sed -i 's/a/b/w x' f", Some(t.path()))
            .expect_err("s///w")
            .construct,
        "sed-s-write-flag",
    );
}

#[test]
fn sed_exec_commands_are_surgically_denied() {
    let t = tmp();
    touch(t.path(), "f");
    assert_eq!(
        resolve_command("sed -i '1e ls' f", Some(t.path()))
            .expect_err("e denied")
            .construct,
        "sed-exec-command",
    );
    assert_eq!(
        resolve_command("sed -i 's/a/b/e' f", Some(t.path()))
            .expect_err("s///e")
            .construct,
        "sed-s-exec-flag",
    );
}

#[test]
fn sed_script_file_and_computed_scripts_are_opaque() {
    let t = tmp();
    touch(t.path(), "f");
    assert_eq!(
        resolve_command("sed -f prog.sed f", Some(t.path()))
            .expect_err("-f")
            .construct,
        "sed-script-file",
    );
    assert_eq!(
        resolve_command("sed -i \"s/$X/y/\" f", Some(t.path()))
            .expect_err("computed")
            .construct,
        "computed-sed-script",
    );
}

#[test]
fn sed_unverifiable_script_fails_closed() {
    let t = tmp();
    touch(t.path(), "f");
    assert_eq!(
        resolve_command("sed -i 'v 4.2' f", Some(t.path()))
            .expect_err("unknown cmd")
            .construct,
        "sed-unverifiable-script",
    );
}

#[test]
fn sed_backup_template_suffix_is_opaque() {
    let t = tmp();
    touch(t.path(), "f");
    assert_eq!(
        resolve_command("sed -i'bak/*' 's/a/b/' f", Some(t.path()))
            .expect_err("template suffix")
            .construct,
        "sed-backup-template",
    );
}

// ── rsync (source-enumerated superset) ───────────────────────────────────────

#[test]
fn rsync_local_file_into_dir() {
    let t = tmp();
    touch(t.path(), "a.txt");
    std::fs::create_dir(t.path().join("dest")).expect("mkdir");
    assert_eq!(
        ok("rsync -av a.txt dest/", t.path()),
        paths(t.path(), &["dest/a.txt"])
    );
}

#[test]
fn rsync_dir_without_slash_lands_under_basename() {
    let t = tmp();
    touch(t.path(), "tree/a");
    touch(t.path(), "tree/sub/b");
    std::fs::create_dir(t.path().join("dst")).expect("mkdir");
    assert_eq!(
        ok("rsync -a tree dst", t.path()),
        paths(t.path(), &["dst/tree", "dst/tree/a", "dst/tree/sub/b"]),
    );
}

#[test]
fn rsync_dir_with_slash_syncs_contents() {
    let t = tmp();
    touch(t.path(), "tree/a");
    touch(t.path(), "tree/sub/b");
    std::fs::create_dir(t.path().join("dst")).expect("mkdir");
    assert_eq!(
        ok("rsync -a --delete tree/ dst", t.path()),
        paths(t.path(), &["dst", "dst/a", "dst/sub/b"]),
    );
}

#[test]
fn rsync_remote_endpoints_are_opaque() {
    let t = tmp();
    touch(t.path(), "a");
    for cmd in [
        "rsync -av host:src .",
        "rsync -av a user@host:dst",
        "rsync -av rsync://host/mod a",
    ] {
        assert_eq!(
            resolve_command(cmd, Some(t.path()))
                .expect_err(cmd)
                .construct,
            "rsync-remote",
            "{cmd}",
        );
    }
}

#[test]
fn rsync_files_from_is_opaque() {
    assert_eq!(
        err("rsync -a --files-from=list.txt src dst", None).construct,
        "rsync-files-from",
    );
}

// ── awk (checkable filter subset) ────────────────────────────────────────────

#[test]
fn awk_pure_filter_is_no_write() {
    let t = tmp();
    // A shell-level redirect still composes through the resolver, but the awk
    // program itself writes nothing.
    for cmd in [
        "awk '{print $1}' a.txt",
        "awk '/a/ && /b/'",
        "awk '/a|b/ {print}'",
        "awk -F: '{print $1}' a.txt",
        "awk -v n=5 '$1 > n {print $2}' a.txt",
        "awk 'BEGIN{if (1 > 0) print \"hi\"}'",
        "gawk '{printf \"%s\\n\", $1}'",
    ] {
        assert_eq!(
            classify(cmd, Some(t.path())),
            SegmentClass::NoWrite,
            "{cmd}"
        );
    }
}

#[test]
fn awk_system_is_surgically_denied() {
    assert_eq!(
        err("awk 'BEGIN{system(\"x\")}'", None).construct,
        "awk-system"
    );
    assert_eq!(
        err("gawk '{system(\"rm \" $1)}'", None).construct,
        "awk-system",
    );
}

#[test]
fn awk_in_program_redirect_is_surgically_denied() {
    assert_eq!(err("awk '{print > \"f\"}'", None).construct, "awk-redirect");
    assert_eq!(
        err("awk '{print $1 >> \"log\"}'", None).construct,
        "awk-redirect",
    );
    assert_eq!(
        err("awk '{printf \"%s\", $1 > \"out\"}'", None).construct,
        "awk-redirect",
    );
}

#[test]
fn awk_command_pipe_is_surgically_denied() {
    assert_eq!(err("awk '{print | \"sort\"}'", None).construct, "awk-pipe");
    assert_eq!(
        err("awk '{\"date\" | getline d; print d}'", None).construct,
        "awk-pipe",
    );
}

#[test]
fn gawk_inplace_records_file_arguments() {
    let t = tmp();
    touch(t.path(), "a.rs");
    touch(t.path(), "b.rs");
    assert_eq!(
        ok("gawk -i inplace '{gsub(/x/,\"y\")}1' a.rs b.rs", t.path()),
        paths(t.path(), &["a.rs", "b.rs"]),
    );
    // A `var=val` operand is an assignment, not a file — awk never edits it.
    assert_eq!(
        ok("gawk -i inplace '{print}' n=5 a.rs", t.path()),
        paths(t.path(), &["a.rs"]),
    );
}

#[test]
fn gawk_inplace_suffix_records_backups() {
    let t = tmp();
    touch(t.path(), "a.rs");
    assert_eq!(
        ok(
            "gawk -i inplace -v INPLACE_SUFFIX=.bak '{print}' a.rs",
            t.path(),
        ),
        paths(t.path(), &["a.rs", "a.rs.bak"]),
    );
}

#[test]
fn awk_program_file_and_computed_and_extension_are_opaque() {
    assert_eq!(
        err("awk -f prog.awk a.txt", None).construct,
        "awk-program-file"
    );
    assert_eq!(
        err("awk \"$PROG\" a.txt", None).construct,
        "awk-computed-program"
    );
    assert_eq!(
        err("gawk -i json '{print}' a.txt", None).construct,
        "awk-extension",
    );
    assert_eq!(
        err("awk -Z '{print}' a.txt", None).construct,
        "awk-unmodeled-flag"
    );
}

// ── perl (checkable substitution subset) ─────────────────────────────────────

#[test]
fn perl_filter_is_no_write() {
    let t = tmp();
    for cmd in [
        "perl -pe 's/a/b/' a.txt",
        "perl -p -e 's/a/b/g; tr/x/y/' a.txt",
        "perl -ne 's/a/b/' a.txt",
        "perl -pe 'y/a-z/A-Z/'",
    ] {
        assert_eq!(
            classify(cmd, Some(t.path())),
            SegmentClass::NoWrite,
            "{cmd}"
        );
    }
}

#[test]
fn perl_inplace_records_files_and_backups() {
    let t = tmp();
    touch(t.path(), "a.rs");
    touch(t.path(), "b.rs");
    assert_eq!(
        ok("perl -i -pe 's/a/b/' a.rs b.rs", t.path()),
        paths(t.path(), &["a.rs", "b.rs"]),
    );
    assert_eq!(
        ok("perl -i.bak -pe 's/a/b/' a.rs", t.path()),
        paths(t.path(), &["a.rs", "a.rs.bak"]),
    );
    // Combined flag cluster.
    assert_eq!(
        ok("perl -pi -e 's/a/b/' a.rs", t.path()),
        paths(t.path(), &["a.rs"]),
    );
}

#[test]
fn perl_lookaround_substitution_is_in_subset() {
    // Look-around is what makes the `catenary sed` retirement lossless.
    let t = tmp();
    touch(t.path(), "a.rs");
    assert_eq!(
        ok("perl -i -pe 's/(?<=bar_)foo/baz/' a.rs", t.path()),
        paths(t.path(), &["a.rs"]),
    );
}

#[test]
fn perl_eval_flag_is_surgically_denied() {
    assert_eq!(
        err("perl -i -pe 's/a/b/e' a.rs", None).construct,
        "perl-e-flag"
    );
    assert_eq!(
        err("perl -pe 's/(\\d+)/$1*2/ge' a.txt", None).construct,
        "perl-e-flag",
    );
}

#[test]
fn perl_beyond_substitution_subset_is_opaque() {
    // `print`-to-handle, `system`, `open` are not the substitution subset.
    assert_eq!(
        err("perl -ne 'print if /x/' a.txt", None).construct,
        "perl-unverifiable-program",
    );
    assert_eq!(
        err("perl -e 'system(\"rm -rf x\")'", None).construct,
        "perl-unverifiable-program",
    );
    assert_eq!(
        err("perl -pe 's/a/b/; open(F,\">z\")' a.txt", None).construct,
        "perl-unverifiable-program",
    );
}

#[test]
fn perl_computed_module_and_program_file_are_opaque() {
    assert_eq!(
        err("perl -pe \"$PROG\" a.txt", None).construct,
        "perl-computed-program"
    );
    assert_eq!(
        err("perl -MFoo -pe 's/a/b/' a.txt", None).construct,
        "perl-module-load"
    );
    assert_eq!(
        err("perl -i edit.pl a.rs", None).construct,
        "perl-program-file"
    );
    assert_eq!(
        err("perl -i'orig_*' -pe 's/a/b/' a.rs", None).construct,
        "perl-backup-template",
    );
}

#[test]
fn perl_script_file_keeps_the_inherited_boundary() {
    assert_eq!(
        resolve_command("perl edit.pl", None)
            .expect("script file ok")
            .writes,
        BTreeSet::new(),
    );
}

// ── Unbounded interpreters: inline code is Opaque, scripts keep the boundary ──

#[test]
fn unbounded_interpreter_inline_code_is_opaque() {
    // ws38 ticket 05: python/ruby/node inline code admits no checkable subset,
    // so its write-set can't be bounded — Opaque at the resolver layer (not just
    // denied by the allowlist), closing the vector where allowlisting the
    // interpreter would let an unattributed write through.
    for cmd in [
        "python -c \"open('f','w').write('x')\"",
        "python3 -c \"open('f','w')\"",
        "ruby -e 'File.write(\"f\",\"x\")'",
        "node -e 'require(\"fs\").writeFileSync(\"f\",\"x\")'",
        "node --eval 'x'",
        "node -p 'x'",
    ] {
        assert_eq!(err(cmd, None).construct, "unbounded-interpreter", "{cmd}");
    }
}

#[test]
fn unbounded_interpreter_script_keeps_the_layer4_boundary() {
    // Plain script execution (no inline-code flag) is an executor — layer-4
    // boundary, NoWrite; the allowlist governs whether it runs.
    let t = tmp();
    let cwd = Some(t.path());
    for cmd in [
        "python script.py",
        "ruby app.rb",
        "node server.js",
        "python",
    ] {
        assert_eq!(classify(cmd, cwd), SegmentClass::NoWrite, "{cmd}");
    }
}

// ── xargs wrapping a checkable interpreter ────────────────────────────────────

#[test]
fn xargs_inplace_interpreter_targets_are_stdin_driven() {
    assert_eq!(
        err("grep -rl old . | xargs perl -i -pe 's/old/new/'", None).construct,
        "stdin-driven-targets",
    );
    assert_eq!(
        err("grep -rl old . | xargs gawk -i inplace '{print}'", None).construct,
        "stdin-driven-targets",
    );
    // A pure-filter interpreter under xargs is a reader — no write.
    assert_eq!(
        resolve_command("echo a | xargs awk '{print}'", None)
            .expect("filter")
            .writes,
        BTreeSet::new(),
    );
    // …but its program is still checked for hazards.
    assert_eq!(
        err("echo a | xargs awk '{system(\"x\")}'", None).construct,
        "awk-system",
    );
}

// ── Shell wrappers over a literal program ────────────────────────────────────

#[test]
fn bash_c_literal_program_recurses() {
    let t = tmp();
    assert_eq!(
        ok("bash -c 'echo x > f.txt'", t.path()),
        paths(t.path(), &["f.txt"])
    );
    assert_eq!(
        ok("sh -c 'make 2>&1 | tee b.log'", t.path()),
        paths(t.path(), &["b.log"])
    );
}

#[test]
fn bash_c_computed_program_is_opaque() {
    assert_eq!(
        err("bash -c \"$PROG\"", None).construct,
        "computed-shell-program"
    );
}

#[test]
fn bash_c_inner_env_is_fresh() {
    // The line's local binding is not exported: the inner `$F` is unbound.
    assert_eq!(
        err("F=out.txt; bash -c 'echo x > $F'", None).construct,
        "unbound-variable",
    );
}

#[test]
fn shell_script_file_keeps_the_inherited_boundary() {
    assert_eq!(
        resolve_command("bash run.sh", None)
            .expect("script file ok")
            .writes,
        BTreeSet::new(),
    );
}

// ── xargs (stdin-driven target lists) ────────────────────────────────────────

#[test]
fn xargs_wrapping_a_writer_is_opaque() {
    let op = err("grep -rl old . | xargs sed -i 's/old/new/'", None);
    assert_eq!(op.construct, "stdin-driven-targets");
    assert!(op.message.contains("sed -i"), "{}", op.message);
    assert_eq!(
        err("echo f | xargs -I{} cp {} dest/", None).construct,
        "stdin-driven-targets"
    );
    assert_eq!(
        err("echo f | xargs tee", None).construct,
        "stdin-driven-targets"
    );
}

#[test]
fn xargs_wrapping_readers_and_deletes_is_free() {
    assert_eq!(
        resolve_command("echo a | xargs cat", None)
            .expect("reader")
            .writes,
        BTreeSet::new()
    );
    assert_eq!(
        resolve_command("echo a | xargs rm -f", None)
            .expect("delete")
            .writes,
        BTreeSet::new()
    );
    assert_eq!(
        resolve_command("echo a | xargs sed -n 'p'", None)
            .expect("pure filter")
            .writes,
        BTreeSet::new(),
    );
}

// ── Unmodeled writers and dynamic executors ──────────────────────────────────

#[test]
fn unmodeled_writers_stay_denied() {
    for cmd in [
        "dd if=/dev/zero of=img",
        "install -m 755 a /usr/bin/a",
        "truncate -s 0 f",
    ] {
        assert_eq!(err(cmd, None).construct, "unmodeled-writer", "{cmd}");
    }
}

#[test]
fn dynamic_executors_are_opaque() {
    assert_eq!(
        err("eval \"echo x > f\"", None).construct,
        "dynamic-execution"
    );
    assert_eq!(err("source setup.sh", None).construct, "dynamic-execution");
}

// ── Loops and compounds ──────────────────────────────────────────────────────

#[test]
fn loop_variable_targets_are_opaque() {
    let op = err("for f in a.rs b.rs; do cp tpl $f; done", None);
    assert_eq!(op.construct, "tainted-variable");
}

#[test]
fn loop_with_literal_target_resolves() {
    let t = tmp();
    assert_eq!(
        ok("for f in a b; do echo $f >> log.txt; done", t.path()),
        paths(t.path(), &["log.txt"]),
    );
}

// ── Composition (union across segments) ──────────────────────────────────────

#[test]
fn sequential_composition_unions_per_segment_sets() {
    // The DESIGN's canonical case: the glob expands before new.rs exists, but
    // the union still covers every actual write because cp's landing is
    // itself recorded.
    let t = tmp();
    touch(t.path(), "tpl.rs");
    touch(t.path(), "src/lib.rs");
    assert_eq!(
        ok("cp tpl.rs src/new.rs && sed -i 's/x/y/' src/*.rs", t.path()),
        paths(t.path(), &["src/new.rs", "src/lib.rs"]),
    );
}

#[test]
fn materialize_then_literal_perl_records_both_files() {
    // Target discovery is materialize-then-literal: `catenary grep -l` finds
    // the paths (a read), then they ride as literal argv into an in-place perl
    // substitution — both files land in the recorded set (DESIGN review-trail 4).
    let t = tmp();
    touch(t.path(), "a.rs");
    touch(t.path(), "b.rs");
    assert_eq!(
        ok(
            "catenary grep -l pat && perl -i -pe 's/a/b/' a.rs b.rs",
            t.path(),
        ),
        paths(t.path(), &["a.rs", "b.rs"]),
    );
}

#[test]
fn segment_classes_compose() {
    let t = tmp();
    // NoWrite ∥ Recorded ∥ PureDelete in one line.
    assert_eq!(
        ok("git status; echo x > f; rm -rf junk", t.path()),
        paths(t.path(), &["f"]),
    );
}

#[test]
fn first_opaque_segment_wins_in_document_order() {
    let op = err("echo x > ok.txt; echo y > $F; echo z > $G", None);
    assert_eq!(op.construct, "unbound-variable");
    assert!(
        op.message.contains("$F") && !op.message.contains("$G"),
        "first opaque segment (F, not G) wins: {}",
        op.message
    );
}

#[test]
fn substitution_writes_join_the_union() {
    let t = tmp();
    assert_eq!(
        ok("echo $(date > stamp.txt)", t.path()),
        paths(t.path(), &["stamp.txt"]),
    );
    // Two levels deep.
    assert_eq!(
        ok("echo $(echo $(date > deep.txt))", t.path()),
        paths(t.path(), &["deep.txt"]),
    );
}

#[test]
fn opaque_write_inside_substitution_denies() {
    assert_eq!(
        err("echo $(date > $STAMP)", None).construct,
        "unbound-variable"
    );
}

// ── Segment classification table ─────────────────────────────────────────────

#[test]
fn segment_class_table() {
    let t = tmp();
    let cwd = Some(t.path());
    // (command, expected class shape)
    assert_eq!(classify("git status", cwd), SegmentClass::NoWrite);
    assert_eq!(classify("cat src/main.rs", cwd), SegmentClass::NoWrite);
    assert_eq!(classify("rm -rf x", cwd), SegmentClass::PureDelete);
    assert_eq!(classify("rmdir empty", cwd), SegmentClass::PureDelete);
    assert!(matches!(
        classify("echo x > f", cwd),
        SegmentClass::Recorded(_)
    ));
    assert!(matches!(
        classify("echo x > $F", cwd),
        SegmentClass::Opaque(_)
    ));
    assert_eq!(classify("catenary grep pat", cwd), SegmentClass::NoWrite);
    assert!(matches!(
        classify("catenary grep pat > hits.txt", cwd),
        SegmentClass::Recorded(_)
    ));
    // Inline interpreter code is Opaque; a plain script executor is NoWrite.
    assert!(matches!(
        classify("python -c 'x'", cwd),
        SegmentClass::Opaque(_)
    ));
    assert_eq!(classify("python script.py", cwd), SegmentClass::NoWrite);
}

// ── expand_word unit coverage ────────────────────────────────────────────────

#[test]
fn expand_word_literal_meta_is_a_literal_path() {
    let t = tmp();
    let state = State::new(Some(t.path()));
    let meta = parse::WordMeta {
        literal_meta: true,
        ..parse::WordMeta::default()
    };
    let got = expand_word("we*rd", meta, &state, Position::Single).expect("literal");
    assert_eq!(got, vec![t.path().join("we*rd")]);
}

#[test]
fn expand_word_empty_is_opaque() {
    let state = State::new(None);
    let got = expand_word("", parse::WordMeta::default(), &state, Position::Single);
    assert_eq!(got.expect_err("empty").construct, "empty-target");
}

// ── git: authorship split (ws38 ticket 03) ───────────────────────────────────

/// Run a git command in `dir`, asserting success. Identity is pinned via env so
/// the fixture doesn't depend on the host's git config.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run git");
    assert!(status.success(), "git {args:?} failed");
}

/// Capture the stdout of a git command in `dir`.
fn git_capture(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git output");
    assert!(out.status.success(), "git {args:?} failed");
    out.stdout
}

/// A fresh git repository in a tempdir, with a pinned identity.
fn git_repo() -> TempDir {
    let t = tmp();
    git(t.path(), &["init", "-q"]);
    git(t.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
    t
}

/// The canonical repository root (git reports realpaths, so query results are
/// joined against the canonicalized root — see [`git_repo`] callers).
fn root_of(t: &TempDir) -> PathBuf {
    t.path().canonicalize().expect("canonicalize repo root")
}

fn write(dir: &Path, rel: &str, body: &str) {
    std::fs::write(dir.join(rel), body).expect("write file");
}

fn commit(dir: &Path, rel: &str, body: &str) {
    write(dir, rel, body);
    git(dir, &["add", rel]);
    git(dir, &["commit", "-qm", "c"]);
}

// Sync rows: allow, no writes, no denial (no git query needed — the resolver
// classifies without executing the navigation).

#[test]
fn git_sync_forms_record_nothing() {
    let t = tmp();
    for cmd in [
        "git pull",
        "git pull origin main",
        "git merge topic",
        "git rebase main",
        "git switch main",
        "git checkout main",
        "git checkout -b feature",
        "git checkout -b feature origin/main",
        "git checkout $BRANCH",
        "git reset --hard",
        "git reset --hard HEAD~1",
        "git reset",
        "git restore src/lib.rs",
        "git restore --staged src/lib.rs",
        "git checkout -- src/lib.rs",
        "git fetch",
        "git fetch --all",
    ] {
        assert_eq!(
            resolve_command(cmd, Some(t.path()))
                .unwrap_or_else(|op| panic!("{cmd:?} should be allowed, got {op:?}"))
                .writes,
            BTreeSet::new(),
            "{cmd}",
        );
    }
}

#[test]
fn git_fetch_and_clone_are_no_write() {
    let t = tmp();
    assert_eq!(
        classify("git fetch --all", Some(t.path())),
        SegmentClass::NoWrite
    );
    assert_eq!(
        classify("git clone https://x/y.git", Some(t.path())),
        SegmentClass::NoWrite
    );
    // Read/write-index subcommands keep the inherited boundary.
    assert_eq!(
        classify("git add -A", Some(t.path())),
        SegmentClass::NoWrite
    );
    assert_eq!(
        classify("git commit -m x", Some(t.path())),
        SegmentClass::NoWrite
    );
}

#[test]
fn git_clean_is_a_pure_delete() {
    let t = tmp();
    assert_eq!(
        classify("git clean -fdx", Some(t.path())),
        SegmentClass::PureDelete
    );
    // A pure-delete git segment carries no line-level debt.
    assert_eq!(
        resolve_command("git clean -fd", Some(t.path()))
            .expect("clean ok")
            .writes,
        BTreeSet::new(),
    );
}

// Content rows: resolve the queried set against a real repository.

#[test]
fn git_stash_pop_records_the_stashed_files() {
    let t = git_repo();
    let root = root_of(&t);
    commit(&root, "a.rs", "one\n");
    write(&root, "a.rs", "two\n");
    git(&root, &["stash", "-q"]);
    assert_eq!(
        ok("git stash pop", &root),
        BTreeSet::from([root.join("a.rs")]),
    );
    // `stash apply` resolves the same way.
    assert_eq!(
        ok("git stash apply", &root),
        BTreeSet::from([root.join("a.rs")]),
    );
}

#[test]
fn git_checkout_pathspec_records_the_diff() {
    let t = git_repo();
    let root = root_of(&t);
    commit(&root, "a.rs", "v1\n");
    commit(&root, "a.rs", "v2\n");
    // HEAD~1 (v1) differs from the working tree (v2) at a.rs.
    assert_eq!(
        ok("git checkout HEAD~1 -- a.rs", &root),
        BTreeSet::from([root.join("a.rs")]),
    );
    // The no-`--` `checkout <ref> <pathspec>` form resolves the same.
    assert_eq!(
        ok("git checkout HEAD~1 a.rs", &root),
        BTreeSet::from([root.join("a.rs")]),
    );
}

#[test]
fn git_restore_source_records_the_diff() {
    let t = git_repo();
    let root = root_of(&t);
    commit(&root, "a.rs", "v1\n");
    commit(&root, "a.rs", "v2\n");
    assert_eq!(
        ok("git restore --source=HEAD~1 a.rs", &root),
        BTreeSet::from([root.join("a.rs")]),
    );
    assert_eq!(
        ok("git restore --source HEAD~1 a.rs", &root),
        BTreeSet::from([root.join("a.rs")]),
    );
}

#[test]
fn git_apply_records_the_numstat_set() {
    let t = git_repo();
    let root = root_of(&t);
    commit(&root, "a.rs", "one\ntwo\n");
    write(&root, "a.rs", "one\nTWO\n");
    let diff = git_capture(&root, &["diff"]);
    std::fs::write(root.join("change.patch"), diff).expect("write patch");
    git(&root, &["checkout", "-q", "--", "a.rs"]);
    assert_eq!(
        ok("git apply change.patch", &root),
        BTreeSet::from([root.join("a.rs")]),
    );
}

// The queries resolve repo-relative results against the *root*, not the cwd —
// proven by running from a subdirectory.

#[test]
fn git_query_paths_resolve_against_repo_root_from_a_subdir() {
    let t = git_repo();
    let root = root_of(&t);
    commit(&root, "top.rs", "one\n");
    std::fs::create_dir(root.join("sub")).expect("mkdir sub");
    write(&root, "top.rs", "two\n");
    git(&root, &["stash", "-q"]);
    let sub = root.join("sub");
    // From `sub`, git reports the stashed path repo-relative ("top.rs"); it must
    // join the repo root, not the cwd.
    let got = ok("git stash pop", &sub);
    assert_eq!(got, BTreeSet::from([root.join("top.rs")]));
    assert!(
        !got.contains(&sub.join("top.rs")),
        "must not resolve against the cwd: {got:?}",
    );
}

// Barrier composition: a sync mover poisons a downstream state query, but not a
// downstream literal target.

#[test]
fn git_sync_barriers_a_downstream_glob() {
    let t = tmp();
    touch(t.path(), "src/lib.rs");
    let op = err(
        "git checkout main && sed -i 's/a/b/' src/*.rs",
        Some(t.path()),
    );
    assert_eq!(op.construct, "git-sync-barrier");
}

#[test]
fn git_sync_leaves_a_downstream_literal_target() {
    let t = tmp();
    assert_eq!(
        ok("git checkout main && sed -i 's/a/b/' src/lib.rs", t.path()),
        paths(t.path(), &["src/lib.rs"]),
    );
}

#[test]
fn git_sync_barrier_poisons_a_git_content_query() {
    let t = git_repo();
    let root = root_of(&t);
    commit(&root, "a.rs", "v1\n");
    commit(&root, "a.rs", "v2\n");
    // The sync sets the barrier; the later content query is then opaque.
    let op = resolve_command(
        "git checkout main && git checkout HEAD~1 -- a.rs",
        Some(&root),
    )
    .expect_err("barriered content query");
    assert_eq!(op.construct, "git-sync-barrier");
}

#[test]
fn git_fetch_does_not_barrier_a_downstream_glob() {
    // fetch writes only `.git/`, so a following glob is still resolvable.
    let t = tmp();
    touch(t.path(), "a.rs");
    touch(t.path(), "b.rs");
    assert_eq!(
        ok("git fetch && sed -i 's/x/y/' *.rs", t.path()),
        paths(t.path(), &["a.rs", "b.rs"]),
    );
}

// Opaque content forms: computed refs/paths and stdin patches deny with a
// teaching message.

#[test]
fn git_computed_ref_in_pathspec_form_is_opaque() {
    let t = git_repo();
    let root = root_of(&t);
    commit(&root, "a.rs", "v1\n");
    assert_eq!(
        resolve_command("git checkout $REF -- a.rs", Some(&root))
            .expect_err("computed ref")
            .construct,
        "git-opaque-ref",
    );
    assert_eq!(
        resolve_command("git restore --source=$REF a.rs", Some(&root))
            .expect_err("computed source")
            .construct,
        "git-opaque-ref",
    );
}

#[test]
fn git_apply_from_stdin_is_opaque() {
    let op = err("git apply", None);
    assert_eq!(op.construct, "git-stdin-patch");
    assert_eq!(
        err("cat patch | git apply", None).construct,
        "git-stdin-patch",
    );
}

#[test]
fn git_content_form_without_cwd_is_opaque() {
    assert_eq!(
        err("git checkout HEAD~1 -- a.rs", None).construct,
        "git-no-cwd",
    );
}

// The standalone `patch` utility.

#[test]
fn patch_records_the_named_target() {
    let t = tmp();
    assert_eq!(
        ok("patch a.rs < changes.diff", t.path()),
        paths(t.path(), &["a.rs"]),
    );
    assert_eq!(
        ok("patch -p1 src/lib.rs < changes.diff", t.path()),
        paths(t.path(), &["src/lib.rs"]),
    );
}

#[test]
fn patch_from_stdin_without_a_target_is_opaque() {
    assert_eq!(
        err("patch -p1 < changes.diff", None).construct,
        "git-stdin-patch"
    );
}

#[test]
fn patch_output_relocation_is_opaque() {
    let t = tmp();
    assert_eq!(
        resolve_command("patch -o out.rs a.rs < changes.diff", Some(t.path()))
            .expect_err("relocated")
            .construct,
        "git-patch-relocated",
    );
}
