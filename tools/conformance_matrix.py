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

Scoping (a recipe/provision-touching PR need not re-conform every server):

- `--base-recipes OLD` / `--base-provision OLD` emit only the stanzas whose parsed
  entry differs from the base (added or changed) — robust to a pin bump inside a
  block, which a header-only diff would miss. Scoping is per-source: a
  recipes-only edit passes an unchanged provisioning base, yielding zero provision
  jobs (diff-scoping for recipe-only edits still works).
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


def main() -> int:
    parser = argparse.ArgumentParser(description="Emit the conformance CI matrix.")
    parser.add_argument("--recipes", type=Path, default=DEFAULT_RECIPES)
    parser.add_argument("--provision", type=Path, default=DEFAULT_PROVISION)
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
        "--only", nargs="*", default=None, help="restrict to these server names"
    )
    args = parser.parse_args()

    recipes = _load(args.recipes, "recipe")
    provisions = _load(args.provision, "provision")

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
