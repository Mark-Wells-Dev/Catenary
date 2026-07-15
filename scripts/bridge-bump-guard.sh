#!/usr/bin/env sh
# bridge-bump-guard.sh — the per-PR bridge-crate version-bump guard.
#
# WHY THIS EXISTS (ws41-03). `crates/catenary-mcp` owns the bridge<->daemon
# wire-protocol definition, and its `version` is the HANDSHAKE COMPARAND: the
# daemon compares the bridge's compiled `catenary-mcp` version against the one it
# links, and any inequality surfaces "run /mcp". That comparand only stays honest
# if the version bumps on EVERY change to the crate. This guard is that
# enforcement.
#
# THE RULE (maintainer-ruled, NO escape hatch). Any diff that touches the
# `catenary-mcp` crate directory — doc-only changes INCLUDED — must bump the
# crate's `version` field in the same diff. There is deliberately no trailer, no
# label, no override of any kind. The ruled asymmetry: a false positive costs one
# harmless `/mcp` per running bridge, while a false negative is a silent wire
# divergence — the exact failure this workstream exists to kill — and an escape
# hatch is a judgment surface through which that failure re-enters (a mis-applied
# "no-wire-change" trailer on a real wire change defeats the guard from inside).
# Mechanical rules also survive out-of-practice reviewers, which bridge reviewers
# will definitionally be.
#
# THE PER-PR GRAIN IS LOAD-BEARING. The maintainer dogfoods nightly and the
# handshake compares at every bridge<->daemon meeting; a release-grained bump
# would leave every nightly build between a mid-cycle wire change and the next
# release carrying a stale claim. So the grain is per-PR, not per-release.
#
# REVISIT TRIGGER. If bridge doc churn ever becomes regular, this guard starts
# paying a false-positive tax (a doc typo forcing a version bump). RE-OPEN THEN,
# NOT BEFORE. Until doc-only churn is regular, the no-escape-hatch asymmetry above
# holds: one harmless /mcp beats one silent wire divergence.
#
# USAGE
#   scripts/bridge-bump-guard.sh <base-ref> [head-ref]
#
# <base-ref>  the diff base — locally `origin/main`, in CI the PR base sha. The
#             comparison is three-dot (base...head), i.e. against the MERGE BASE,
#             matching the `origin/main...HEAD` a PR diff shows.
# [head-ref]  the head to compare (default: HEAD). Explicit so the target is
#             testable against synthetic commits in a worktree.
#
# EXIT STATUS
#   0  crate untouched, OR touched and bumped — the healthy states.
#   1  crate touched WITHOUT a version bump — the failure this guard exists for.
#   2  usage / environment error (bad ref, not a git repo, unreadable Cargo.toml).

set -eu

CRATE_DIR="crates/catenary-mcp"
CRATE_MANIFEST="$CRATE_DIR/Cargo.toml"

die_usage() {
	printf 'bridge-bump-guard: %s\n' "$1" >&2
	printf 'usage: %s <base-ref> [head-ref]\n' "$0" >&2
	exit 2
}

[ "$#" -ge 1 ] || die_usage "missing <base-ref>"
BASE="$1"
HEAD_REF="${2:-HEAD}"

git rev-parse --git-dir >/dev/null 2>&1 || die_usage "not a git repository"
git rev-parse --verify --quiet "$BASE^{commit}" >/dev/null \
	|| die_usage "base ref '$BASE' does not resolve to a commit"
git rev-parse --verify --quiet "$HEAD_REF^{commit}" >/dev/null \
	|| die_usage "head ref '$HEAD_REF' does not resolve to a commit"

# Did the diff touch the bridge crate at all? "Touches" = ANY path under the
# crate directory (source, Cargo.toml, docs, fixtures — everything). Three-dot
# diff = base..MERGE_BASE(base,head)..head, the same set a PR shows.
changed="$(git diff --name-only "$BASE...$HEAD_REF" -- "$CRATE_DIR")"

if [ -z "$changed" ]; then
	printf 'bridge-bump-guard: %s untouched between %s...%s — OK\n' \
		"$CRATE_DIR" "$BASE" "$HEAD_REF"
	exit 0
fi

printf 'bridge-bump-guard: %s touched between %s...%s:\n' \
	"$CRATE_DIR" "$BASE" "$HEAD_REF"
printf '%s\n' "$changed" | while IFS= read -r f; do
	printf '  %s\n' "$f"
done

# The crate was touched. The bump check is a REAL version change, not just any
# Cargo.toml edit: the `version` field must differ between base and head.
version_at() {
	# $1 = ref. Reads the crate manifest at that ref and extracts the first
	# `version = "..."` line in the [package] table. The manifest opens with
	# [package] then `version`, so the first match is the package version.
	git show "$1:$CRATE_MANIFEST" 2>/dev/null \
		| sed -n 's/^version *= *"\([^"]*\)".*/\1/p' \
		| head -1
}

# The merge base is the true "before" the three-dot diff compares against; use it
# so a base that has advanced past the branch point cannot mask an in-branch bump.
MERGE_BASE="$(git merge-base "$BASE" "$HEAD_REF")"
base_version="$(version_at "$MERGE_BASE")"
head_version="$(version_at "$HEAD_REF")"

[ -n "$base_version" ] || die_usage "could not read version from $CRATE_MANIFEST at merge-base $MERGE_BASE"
[ -n "$head_version" ] || die_usage "could not read version from $CRATE_MANIFEST at $HEAD_REF"

if [ "$base_version" = "$head_version" ]; then
	printf 'bridge-bump-guard: FAIL\n' >&2
	printf '  %s was touched but its version did not change (%s at both %s and %s).\n' \
		"$CRATE_DIR" "$base_version" "$MERGE_BASE" "$HEAD_REF" >&2
	printf '  The bridge crate version is the daemon<->bridge handshake comparand:\n' >&2
	printf '  every change to this crate — docs included — must bump\n' >&2
	printf '  %s so a running bridge is never left claiming a stale version.\n' \
		"$CRATE_MANIFEST" >&2
	printf '  There is no override. Bump the version.\n' >&2
	exit 1
fi

printf 'bridge-bump-guard: OK — version bumped %s -> %s\n' "$base_version" "$head_version"
exit 0
