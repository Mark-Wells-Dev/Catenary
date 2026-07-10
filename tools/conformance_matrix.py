#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Mark Wells <contact@markwells.dev>
"""Emit the GitHub Actions conformance matrix from recipes AND provisioning.

The `discover` job of `.github/workflows/conformance.yml` runs this to turn
`defaults/recipes.toml` (user-grade install recipes) AND
`defaults/ci-provision.toml` (CI-only provisioning: toolchain components, system
packages, checksummed GitHub-release binaries, git-pinned source builds, gems)
into one matrix entry per server the harness should install-and-conform.

Each entry carries a `source` (`recipe` | `provision`) that the workflow branches
on, plus the fields that source's install step needs:

    recipe:    { server, source, ecosystem, package, version, tier, hash,
                 runtime_name, runtime_version }
    provision: { server, source, kind, version, component, apt, repo, asset,
                 sha256, bin, git, rev, url, gem, runtime_name, runtime_version }

A provisioning stanza marked `pending` (a required pin that could not be resolved
mechanically — never invented) is SKIPPED with a stderr note: it cannot be
installed, so emitting it would create a guaranteed-red job that blocks blessing.

The platform dimension (misc 164): `--platform linux` (the default) emits the
matrix above; `--platform macos` emits the Homebrew leg from
`defaults/ci-provision-macos.toml` instead:

    macos:     { server, source, kind, formula, bin }

Before emitting, the macOS file is validated as a PARTITION of the
Linux-conformed set: every server the Linux matrix conforms appears exactly once
— either `kind = "homebrew"` + `formula`, or an explicit `skip = true` with a
`note` — so a server the macOS matrix does not prove is never silently absent.
A violation exits nonzero (the discover job fails loudly); skips are reported on
stderr. The identical check gates `make check`
(tests/conformance_harness.rs `macos_provisioning_partitions_the_conformed_set`).

Scoping (a recipe/provision-touching PR need not re-conform every server):

- `--base-recipes OLD` / `--base-provision OLD` emit only the stanzas whose parsed
  entry differs from the base (added or changed) — robust to a pin bump inside a
  block, which a header-only diff would miss. Scoping is per-source: a
  recipes-only edit passes an unchanged provisioning base, yielding zero provision
  jobs (diff-scoping for recipe-only edits still works). `--base-macos-provision
  OLD` is the macOS twin (only meaningful with `--platform macos`).
- `--only NAME ...` restricts to the named servers across both sources.
- with neither base, the full matrix is emitted (structural / refresh PRs, where
  every pin can move).

Output is a single JSON object `{"include": [...]}` on stdout, ready for
`fromJson()`; a human-readable count goes to stderr.
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
from pathlib import Path

DEFAULTS = Path(__file__).resolve().parent.parent / "defaults"
DEFAULT_RECIPES = DEFAULTS / "recipes.toml"
DEFAULT_PROVISION = DEFAULTS / "ci-provision.toml"
DEFAULT_MACOS_PROVISION = DEFAULTS / "ci-provision-macos.toml"


def _load(path: Path, table: str) -> dict[str, dict]:
    """Parse `path` and return its top-level `[<table>.*]` map (empty if absent)."""
    return tomllib.loads(path.read_text()).get(table, {})


def _scoped(entries: dict[str, dict], base: Path | None, table: str) -> set[str]:
    """Names to include: changed-vs-base when a base is given, else all."""
    if base is None:
        return set(entries)
    base_entries = _load(base, table)
    return {n for n, e in entries.items() if base_entries.get(n) != e}


def recipe_entry(name: str, recipe: dict) -> dict:
    """One `source = recipe` matrix entry."""
    runtime = recipe.get("runtime") or {}
    return {
        "server": name,
        "source": "recipe",
        "ecosystem": recipe.get("ecosystem", ""),
        "package": recipe.get("package", ""),
        "version": recipe.get("version", ""),
        "tier": recipe.get("tier", ""),
        "hash": recipe.get("hash", ""),
        "runtime_name": runtime.get("name", ""),
        "runtime_version": runtime.get("version", ""),
    }


def provision_entry(name: str, prov: dict) -> dict:
    """One `source = provision` matrix entry (all kind-specific fields flattened)."""
    runtime = prov.get("runtime") or {}
    return {
        "server": name,
        "source": "provision",
        "kind": prov.get("kind", ""),
        "version": prov.get("version", ""),
        "component": prov.get("component", ""),
        "apt": prov.get("apt", ""),
        "repo": prov.get("repo", ""),
        "asset": prov.get("asset", ""),
        "sha256": prov.get("sha256", ""),
        "bin": prov.get("bin", ""),
        "git": prov.get("git", ""),
        "rev": prov.get("rev", ""),
        "url": prov.get("url", ""),
        "gem": prov.get("gem", ""),
        "runtime_name": runtime.get("name", ""),
        "runtime_version": runtime.get("version", ""),
    }


def macos_entry(name: str, prov: dict) -> dict:
    """One macOS matrix entry (always `source = provision`, `kind = homebrew`)."""
    return {
        "server": name,
        "source": "provision",
        "kind": prov.get("kind", ""),
        "formula": prov.get("formula", ""),
        "bin": prov.get("bin", ""),
    }


def conformed_names(recipes: dict[str, dict], provisions: dict[str, dict]) -> set[str]:
    """The Linux-conformed set — the same filter `src/recipes.rs`
    `conformed_server_names` applies: every recipe or provision that is neither
    `pending` nor `conformance = false`."""
    conformed = {n for n, r in recipes.items() if r.get("conformance", True)}
    conformed |= {
        n
        for n, p in provisions.items()
        if not p.get("pending") and p.get("conformance", True)
    }
    return conformed


def validate_macos(macos: dict[str, dict], conformed: set[str]) -> list[str]:
    """Partition errors: the macOS file must cover the Linux-conformed set exactly.

    Every Linux-conformed server appears exactly once — either provisioned
    (`kind = "homebrew"` + `formula`) or an explicit `skip = true` with a `note`
    — so a server the macOS matrix does not prove is never silently absent
    (misc 164). Mirrors the `make check` guard in tests/conformance_harness.rs.
    """
    errors = []
    for name in sorted(conformed - set(macos)):
        errors.append(
            f"macOS provisioning is silently missing `{name}` — add a homebrew "
            f"stanza or an explicit `skip = true` with a note"
        )
    for name in sorted(set(macos) - conformed):
        errors.append(
            f"macOS provisioning names `{name}`, which the Linux matrix does not "
            f"conform (no non-pending, non-exempt recipe/provision)"
        )
    for name, prov in sorted(macos.items()):
        if prov.get("skip"):
            if not str(prov.get("note", "")).strip():
                errors.append(f"macOS skip for `{name}` carries no honest `note`")
        else:
            if prov.get("kind") != "homebrew":
                errors.append(f'macOS provision `{name}` must be kind = "homebrew"')
            if not str(prov.get("formula", "")).strip():
                errors.append(f"macOS provision `{name}` names no `formula`")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser(description="Emit the conformance CI matrix.")
    parser.add_argument("--recipes", type=Path, default=DEFAULT_RECIPES)
    parser.add_argument("--provision", type=Path, default=DEFAULT_PROVISION)
    parser.add_argument("--macos-provision", type=Path, default=DEFAULT_MACOS_PROVISION)
    parser.add_argument(
        "--platform",
        choices=["linux", "macos"],
        default="linux",
        help="which platform leg to emit (misc 164)",
    )
    parser.add_argument(
        "--base-recipes",
        type=Path,
        default=None,
        help="only include recipes changed vs this base recipes.toml",
    )
    parser.add_argument(
        "--base-provision",
        type=Path,
        default=None,
        help="only include provisions changed vs this base ci-provision.toml",
    )
    parser.add_argument(
        "--base-macos-provision",
        type=Path,
        default=None,
        help="only include macOS provisions changed vs this base ci-provision-macos.toml",
    )
    parser.add_argument(
        "--only", nargs="*", default=None, help="restrict to these server names"
    )
    args = parser.parse_args()

    recipes = _load(args.recipes, "recipe")
    provisions = _load(args.provision, "provision")

    if args.platform == "macos":
        macos = _load(args.macos_provision, "provision")
        errors = validate_macos(macos, conformed_names(recipes, provisions))
        if errors:
            for error in errors:
                print(f"error: {error}", file=sys.stderr)
            return 1
        names = _scoped(macos, args.base_macos_provision, "provision")
        if args.only:
            names &= set(args.only)
        include = []
        skipped = []
        for name in sorted(names):
            prov = macos[name]
            if prov.get("skip"):
                skipped.append((name, str(prov.get("note", "")).strip()))
                continue
            include.append(macos_entry(name, prov))
        print(json.dumps({"include": include}))
        print(
            f"macOS conformance matrix: {len(include)} job(s) (homebrew)",
            file=sys.stderr,
        )
        for name, note in skipped:
            print(f"skip (macos): `{name}` — {note}", file=sys.stderr)
        return 0

    recipe_names = _scoped(recipes, args.base_recipes, "recipe")
    provision_names = _scoped(provisions, args.base_provision, "provision")

    if args.only:
        only = set(args.only)
        recipe_names &= only
        provision_names &= only

    # `conformance = false` (default true) excludes a server the shipped
    # lifecycle cannot conform (no diagnostics at all, or a debounced/scan-based
    # publish that never lands in the settle window). Dropping it here — the same
    # filter the Rust matrix<->CASES drift guard applies — keeps a guaranteed-red
    # job out of the matrix; the recipe/provision `note` records the honest reason.
    include = []
    skipped_exempt = []
    for name in sorted(recipe_names):
        recipe = recipes[name]
        if not recipe.get("conformance", True):
            skipped_exempt.append(name)
            continue
        include.append(recipe_entry(name, recipe))
    skipped_pending = []
    for name in sorted(provision_names):
        prov = provisions[name]
        if prov.get("pending"):
            skipped_pending.append(name)
            continue
        if not prov.get("conformance", True):
            skipped_exempt.append(name)
            continue
        include.append(provision_entry(name, prov))

    print(json.dumps({"include": include}))
    recipe_jobs = sum(1 for e in include if e["source"] == "recipe")
    provision_jobs = sum(1 for e in include if e["source"] == "provision")
    print(
        f"conformance matrix: {len(include)} job(s) "
        f"({recipe_jobs} recipe, {provision_jobs} provision)",
        file=sys.stderr,
    )
    for name in skipped_pending:
        print(f"skip: provision `{name}` is pending an unresolved pin", file=sys.stderr)
    for name in sorted(skipped_exempt):
        print(f"skip: `{name}` is conformance = false (cannot conform)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
