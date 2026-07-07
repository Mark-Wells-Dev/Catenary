#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Mark Wells <contact@markwells.dev>
"""Emit the GitHub Actions conformance matrix from the install recipes.

The `discover` job of `.github/workflows/conformance.yml` runs this to turn
`defaults/recipes.toml` into one matrix entry per server the harness should
install-and-conform. Each entry carries everything a matrix job needs to install
the pinned artifact and select it in the harness:

    { server, ecosystem, package, version, tier, hash, runtime_name, runtime_version }

Scoping (a recipe-touching PR need not re-conform every server):

- `--base OLD_RECIPES` emits only the recipes whose parsed entry differs from
  `OLD_RECIPES` (added or changed) — robust to a version/hash bump inside a
  block, which a header-only diff would miss.
- `--only NAME ...` restricts to the named recipes.
- with neither, the full matrix is emitted (structural changes and
  refresh-recipes PRs, where every pin can move).

Output is a single JSON object `{"include": [...]}` on stdout, ready for
`fromJson()`; a human-readable count goes to stderr.
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
from pathlib import Path

DEFAULT_RECIPES = Path(__file__).resolve().parent.parent / "defaults" / "recipes.toml"


def main() -> int:
    parser = argparse.ArgumentParser(description="Emit the conformance CI matrix.")
    parser.add_argument("--recipes", type=Path, default=DEFAULT_RECIPES)
    parser.add_argument(
        "--base",
        type=Path,
        default=None,
        help="only include recipes changed vs this base recipes.toml",
    )
    parser.add_argument(
        "--only", nargs="*", default=None, help="restrict to these recipe names"
    )
    args = parser.parse_args()

    doc = tomllib.loads(args.recipes.read_text())
    recipes: dict[str, dict] = doc.get("recipe", {})

    if args.base is not None:
        base = tomllib.loads(args.base.read_text()).get("recipe", {})
        names = sorted(n for n, r in recipes.items() if base.get(n) != r)
    elif args.only:
        names = args.only
    else:
        names = sorted(recipes)
    include = []
    for name in names:
        recipe = recipes.get(name)
        if recipe is None:
            print(f"warn: no [recipe.{name}] — skipped", file=sys.stderr)
            continue
        runtime = recipe.get("runtime") or {}
        include.append(
            {
                "server": name,
                "ecosystem": recipe.get("ecosystem", ""),
                "package": recipe.get("package", ""),
                "version": recipe.get("version", ""),
                "tier": recipe.get("tier", ""),
                "hash": recipe.get("hash", ""),
                "runtime_name": runtime.get("name", ""),
                "runtime_version": runtime.get("version", ""),
            }
        )

    print(json.dumps({"include": include}))
    print(f"conformance matrix: {len(include)} job(s)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
