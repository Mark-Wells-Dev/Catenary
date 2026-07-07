#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Mark Wells <contact@markwells.dev>
"""Re-resolve, re-hash, and rewrite the conformance install-recipe pins.

`make refresh-recipes` runs this then prints the reviewable diff. It PREPARES a
proposed pin bump; it never merges — the merge policy (mechanical gates,
auto-merge on green) is tui-rework 08's concern. This script only rewrites
`defaults/recipes.toml` and leaves the working tree dirty for review.

For each `[recipe.<name>]` it re-resolves the latest version and, for the
hash-carrying npm tier, the registry `dist.integrity` (SRI sha512), then edits
that recipe's `version` (and `hash`) lines in place — comments, ordering, and
every other stanza are preserved, so the diff is small and readable.

Only the four registry HTTP metadata APIs are used (npm, crates.io, PyPI, the Go
module proxy) via the Python standard library — no `npm`/`cargo`/`pip`/`go` CLI,
no `curl`/`jq`. A hash is therefore never hand-computed: npm integrity is copied
verbatim from the registry, exactly as the drafts were authored.

Offline tolerance: each recipe resolves independently inside try/except. A
network failure for one server is reported and that recipe keeps its current pin
— never a partially-corrupt file. The rewrite is computed entirely in memory and
written once, atomically, only after every recipe has been attempted.

pip note: the `pip-hashes` tier's `--require-hashes` needs the sha256 of the
whole resolved closure (`pip-compile --generate-hashes`), which a single
metadata fetch cannot produce. This script bumps the pip *version* and leaves
the hash for that step, rather than half-filling it.

Usage:
    python3 tools/refresh_recipes.py [--recipes PATH] [--only NAME ...] [--check]

    --check exits non-zero if any pin would change (a CI "is this in sync?"
    probe), without writing.
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

DEFAULT_RECIPES = Path(__file__).resolve().parent.parent / "defaults" / "recipes.toml"
USER_AGENT = "catenary-refresh-recipes (+https://github.com/TwoWells/Catenary)"
TIMEOUT = 30


def fetch_json(url: str) -> dict:
    """GETs `url` and parses JSON, with a descriptive User-Agent."""
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:  # noqa: S310 (https only)
        return json.load(resp)


def resolve_npm(package: str) -> tuple[str, str | None]:
    """Latest version + SRI sha512 integrity from the npm registry."""
    latest = fetch_json(f"https://registry.npmjs.org/{package}/latest")
    version = latest["version"]
    meta = fetch_json(f"https://registry.npmjs.org/{package}/{version}")
    integrity = meta.get("dist", {}).get("integrity")
    return version, integrity


def resolve_cargo(package: str) -> tuple[str, str | None]:
    """Latest stable version from crates.io (hash delegated to `--locked`)."""
    data = fetch_json(f"https://crates.io/api/v1/crates/{package}")
    return data["crate"]["max_stable_version"], None


def resolve_pip(package: str) -> tuple[str, str | None]:
    """Latest version from PyPI (hash left to `pip-compile --generate-hashes`)."""
    data = fetch_json(f"https://pypi.org/pypi/{package}/json")
    return data["info"]["version"], None


def _go_escape(module: str) -> str:
    """Go module-proxy case-encoding: an uppercase letter becomes `!<lower>`."""
    return "".join(f"!{c.lower()}" if c.isupper() else c for c in module)


def resolve_go(module: str) -> tuple[str, str | None]:
    """Latest version from the Go module proxy (hash via the checksum DB)."""
    data = fetch_json(f"https://proxy.golang.org/{_go_escape(module)}/@latest")
    return data["Version"], None


RESOLVERS = {
    "npm": resolve_npm,
    "cargo": resolve_cargo,
    "pip": resolve_pip,
    "go": resolve_go,
}


def block_span(lines: list[str], name: str) -> tuple[int, int] | None:
    """Returns [start, end) line indices of the `[recipe.<name>]` table body.

    `start` is the header line; `end` is the first line of the next top-level
    table (or EOF). Editing stays inside this span so no sibling recipe is
    touched.
    """
    header = f"[recipe.{name}]"
    start = next((i for i, ln in enumerate(lines) if ln.strip() == header), None)
    if start is None:
        return None
    end = len(lines)
    for i in range(start + 1, len(lines)):
        if lines[i].lstrip().startswith("["):
            end = i
            break
    return start, end


def edit_pin(lines: list[str], name: str, version: str, hash_: str | None) -> bool:
    """Rewrites the `version`/`hash` lines of one recipe. Returns True if changed.

    `version` is always set. `hash` is set only when provided; if the recipe has
    no `hash` line yet, one is inserted immediately after `version`.
    """
    span = block_span(lines, name)
    if span is None:
        raise KeyError(f"no [recipe.{name}] block found")
    start, end = span
    changed = False
    saw_hash = False
    for i in range(start, end):
        stripped = lines[i].lstrip()
        if stripped.startswith("version"):
            new = f'version = "{version}"\n'
            if lines[i] != new:
                lines[i] = new
                changed = True
        elif stripped.startswith("hash") and hash_ is not None:
            saw_hash = True
            new = f'hash = "{hash_}"\n'
            if lines[i] != new:
                lines[i] = new
                changed = True
    if hash_ is not None and not saw_hash:
        # Insert a hash line right after the version line.
        for i in range(start, end):
            if lines[i].lstrip().startswith("version"):
                lines.insert(i + 1, f'hash = "{hash_}"\n')
                changed = True
                break
    return changed


def main() -> int:
    parser = argparse.ArgumentParser(description="Refresh conformance recipe pins.")
    parser.add_argument("--recipes", type=Path, default=DEFAULT_RECIPES)
    parser.add_argument(
        "--only", nargs="*", default=None, help="restrict to these recipe names"
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="exit non-zero if any pin would change; do not write",
    )
    args = parser.parse_args()

    text = args.recipes.read_text()
    doc = tomllib.loads(text)
    recipes = doc.get("recipe", {})
    lines = text.splitlines(keepends=True)

    selected = args.only if args.only else sorted(recipes)
    changed_names: list[str] = []
    failures: list[str] = []

    for name in selected:
        recipe = recipes.get(name)
        if recipe is None:
            failures.append(f"{name}: not found in {args.recipes.name}")
            continue
        ecosystem = recipe.get("ecosystem")
        package = recipe.get("package")
        resolver = RESOLVERS.get(ecosystem)
        if resolver is None or not package:
            failures.append(f"{name}: unknown ecosystem/package ({ecosystem}/{package})")
            continue
        try:
            version, hash_ = resolver(package)
        except (urllib.error.URLError, TimeoutError, KeyError, OSError) as exc:
            failures.append(f"{name}: resolve failed — {exc}")
            continue
        old_version = recipe.get("version")
        try:
            if edit_pin(lines, name, version, hash_):
                marker = "" if old_version == version else f" {old_version} -> {version}"
                changed_names.append(f"{name}{marker}")
        except KeyError as exc:
            failures.append(f"{name}: {exc}")

    new_text = "".join(lines)
    would_change = new_text != text

    if args.check:
        for line in changed_names:
            print(f"stale: {line}", file=sys.stderr)
        for line in failures:
            print(f"warn:  {line}", file=sys.stderr)
        return 1 if would_change else 0

    if would_change:
        tmp = args.recipes.with_suffix(".toml.tmp")
        tmp.write_text(new_text)
        tmp.replace(args.recipes)

    print(f"refresh-recipes: {len(changed_names)} pin(s) updated", file=sys.stderr)
    for line in changed_names:
        print(f"  updated: {line}", file=sys.stderr)
    for line in failures:
        print(f"  kept (unresolved): {line}", file=sys.stderr)
    if would_change:
        print("Review the change: git --no-pager diff -- defaults/recipes.toml", file=sys.stderr)
    else:
        print("  all pins already current", file=sys.stderr)
    # Failures are non-fatal (offline tolerance) — the file is coherent either way.
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
