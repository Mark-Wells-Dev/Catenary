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
                 co_install, artifact_url, artifact_hash, artifact_bin,
                 runtime_name, runtime_version }
    provision: { server, source, kind, version, component, apt, repo, asset,
                 sha256, bin, git, rev, url, gem, runtime_name, runtime_version }

The `artifact_*` fields (lsm 04) are the recipe's pinned linux-x86_64 official
upstream release binary — url + SRI hash + launcher path — resolved from its
`[recipe.<server>.artifact.linux-x86_64]` table, empty when the recipe pins no
binary for the leg's platform. The install step PREFERS a non-empty
artifact_url over the ecosystem install, mirroring the engine
(src/install.rs `InstallPlan::resolve`).

`co_install` (misc 195) is a list of `{package, version, hash}` pinned npm
packages the server needs at runtime but does not bundle
(typescript-language-server → typescript). The install step fetches, verifies,
and installs each alongside the server by the same npm-tarball-sha512 mechanics,
so the gate rides a KNOWN version, not the runner image's ambient one. Empty for
every other recipe.

A provisioning stanza marked `pending` (a required pin that could not be resolved
mechanically — never invented) is SKIPPED with a stderr note: it cannot be
installed, so emitting it would create a guaranteed-red job that blocks blessing.

The platform dimension (misc 164): `--platform linux` (the default) emits the
matrix above; `--platform macos` emits the macOS leg from
`defaults/ci-provision-macos.toml` instead. The macOS leg is brew-primary, but a
server with no homebrew-core formula rides its platform-neutral Linux kind,
REFERENCED (not re-pinned) so a Linux pin bump cannot diverge — the generator
resolves the reference against recipes.toml / ci-provision.toml when emitting the
entry:

    macos homebrew:        { server, source, kind, formula, bin }
    macos linux-recipe:    { server, source, kind, ecosystem, package, version,
                             hash, co_install }
    macos linux-provision: { server, source, kind, prov_kind, git, rev, version }

Before emitting, the macOS file is validated as a PARTITION of the
Linux-conformed set: every server the Linux matrix conforms appears exactly once
— `kind = "homebrew"` + `formula`, a `linux-recipe` / `linux-provision`
reference that resolves against the Linux source, or an explicit `skip = true`
with a `note` — so a server the macOS matrix does not prove is never silently
absent. A violation exits nonzero (the discover job fails loudly); skips are
reported on stderr. The identical check gates `make check`
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


def co_installs(recipe: dict) -> list[dict]:
    """The recipe's pinned npm co-installs (misc 195), each `{package, version,
    hash}`, or `[]`.

    Carried on the matrix entry (Linux recipe AND macOS linux-recipe) so the
    install step can fetch, verify, and install each one alongside the server by
    the same npm-tarball-sha512 mechanics — a KNOWN co-installed version on both
    platforms instead of the runner image's ambient one.
    """
    return [
        {
            "package": co.get("package", ""),
            "version": co.get("version", ""),
            "hash": co.get("hash", ""),
        }
        for co in recipe.get("co_install", [])
    ]


def recipe_entry(name: str, recipe: dict) -> dict:
    """One `source = recipe` matrix entry."""
    runtime = recipe.get("runtime") or {}
    # The lsm-04 binary shape: the Linux leg's pinned official release binary,
    # when the recipe carries one. Preferred by the install step over the
    # ecosystem, mirroring the engine's platform preference.
    artifact = recipe.get("artifact", {}).get("linux-x86_64", {})
    return {
        "server": name,
        "source": "recipe",
        "ecosystem": recipe.get("ecosystem", ""),
        "package": recipe.get("package", ""),
        "version": recipe.get("version", ""),
        "tier": recipe.get("tier", ""),
        "hash": recipe.get("hash", ""),
        "co_install": co_installs(recipe),
        "artifact_url": artifact.get("url", ""),
        "artifact_hash": artifact.get("hash", ""),
        "artifact_bin": artifact.get("bin", ""),
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


# The three macOS provisioning kinds (misc 164). `homebrew` provisions via a
# homebrew-core formula; the two neutral kinds REFERENCE this server's Linux
# source so a Linux pin bump flows through without a second pin to maintain.
MACOS_HOMEBREW = "homebrew"
MACOS_LINUX_RECIPE = "linux-recipe"
MACOS_LINUX_PROVISION = "linux-provision"
MACOS_KINDS = frozenset({MACOS_HOMEBREW, MACOS_LINUX_RECIPE, MACOS_LINUX_PROVISION})


def macos_entry(
    name: str,
    prov: dict,
    recipes: dict[str, dict],
    provisions: dict[str, dict],
) -> dict:
    """One macOS matrix entry.

    A `homebrew` stanza carries the formula/bin directly. A `linux-recipe` or
    `linux-provision` stanza carries no pin of its own — the reference is this
    server's key — so the fields the conform-macos install step needs are
    RESOLVED here from the Linux source (defaults/recipes.toml for a recipe,
    defaults/ci-provision.toml for a provision). Every key is present (empty
    where a kind does not use it) so a `matrix.<field>` reference never errors on
    a job of the other kind, mirroring the Linux matrix.
    """
    kind = prov.get("kind", "")
    entry = {
        "server": name,
        "source": "provision",
        "kind": kind,
        "formula": "",
        "bin": "",
        "ecosystem": "",
        "package": "",
        "version": "",
        "hash": "",
        "co_install": [],
        "prov_kind": "",
        "git": "",
        "rev": "",
    }
    if kind == MACOS_HOMEBREW:
        entry["formula"] = prov.get("formula", "")
        entry["bin"] = prov.get("bin", "")
    elif kind == MACOS_LINUX_RECIPE:
        recipe = recipes[name]
        entry["ecosystem"] = recipe.get("ecosystem", "")
        entry["package"] = recipe.get("package", "")
        entry["version"] = recipe.get("version", "")
        entry["hash"] = recipe.get("hash", "")
        # A linux-recipe rides the Linux npm recipe, so its co-installs flow
        # through here too (misc 195) — a Linux pin bump reaches macOS with no
        # second pin to maintain.
        entry["co_install"] = co_installs(recipe)
    elif kind == MACOS_LINUX_PROVISION:
        source = provisions[name]
        entry["prov_kind"] = source.get("kind", "")
        entry["git"] = source.get("git", "")
        entry["rev"] = source.get("rev", "")
        entry["version"] = source.get("version", "")
    return entry


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


def validate_macos(
    macos: dict[str, dict],
    conformed: set[str],
    recipes: dict[str, dict],
    provisions: dict[str, dict],
) -> list[str]:
    """Partition errors: the macOS file must cover the Linux-conformed set exactly.

    Every Linux-conformed server appears exactly once — brew-provisioned
    (`kind = "homebrew"` + `formula`), a `linux-recipe` / `linux-provision`
    reference that resolves against the Linux source, or an explicit
    `skip = true` with a `note` — so a server the macOS matrix does not prove is
    never silently absent (misc 164). Mirrors the `make check` guard in
    tests/conformance_harness.rs.
    """
    errors = []
    for name in sorted(conformed - set(macos)):
        errors.append(
            f"macOS provisioning is silently missing `{name}` — add a homebrew "
            f"stanza, a linux-recipe/linux-provision reference, or an explicit "
            f"`skip = true` with a note"
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
            continue
        kind = prov.get("kind")
        if kind not in MACOS_KINDS:
            errors.append(
                f'macOS provision `{name}` has kind `{kind}` — must be one of '
                f'"homebrew" / "linux-recipe" / "linux-provision"'
            )
        elif kind == MACOS_HOMEBREW:
            if not str(prov.get("formula", "")).strip():
                errors.append(f"macOS provision `{name}` names no `formula`")
        elif kind == MACOS_LINUX_RECIPE:
            if name not in recipes:
                errors.append(
                    f"macOS provision `{name}` is `linux-recipe` but names no "
                    f"recipe in defaults/recipes.toml"
                )
            elif recipes[name].get("ecosystem") == "binary":
                # A binary recipe pins per-platform artifacts; the macOS leg has
                # no binary install branch, and a Linux artifact could never
                # conform macOS anyway — a guaranteed-red job stays out of the
                # matrix (lsm 04).
                errors.append(
                    f"macOS provision `{name}` is `linux-recipe` but the Linux "
                    f"recipe is ecosystem `binary` (per-platform artifacts are "
                    f"not platform-neutral) — use a homebrew stanza or an "
                    f"explicit skip"
                )
        elif kind == MACOS_LINUX_PROVISION and name not in provisions:
            errors.append(
                f"macOS provision `{name}` is `linux-provision` but names no "
                f"stanza in defaults/ci-provision.toml"
            )
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
        errors = validate_macos(
            macos, conformed_names(recipes, provisions), recipes, provisions
        )
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
            include.append(macos_entry(name, prov, recipes, provisions))
        print(json.dumps({"include": include}))
        brew_jobs = sum(1 for e in include if e["kind"] == MACOS_HOMEBREW)
        neutral_jobs = len(include) - brew_jobs
        print(
            f"macOS conformance matrix: {len(include)} job(s) "
            f"({brew_jobs} homebrew, {neutral_jobs} linux-reference)",
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
