#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Mark Wells <contact@markwells.dev>
"""Daily continuous-advisory re-scan of CURRENTLY pinned versions (tui-rework 08).

STAGED, NOT LIVE. Belongs in the external registry repo (see `registry/README.md`).

Compromise is often discovered *after* a version is pinned, so a green
gate at pin time is not forever. This script OSV-checks every version the registry
currently pins and, on an advisory hit, rolls the pin back to the most recent
KNOWN-CLEAN predecessor (re-checked against OSV so a rollback is never into
another advised version) and emits the rollback diff. The fleet is safe at its
next registry fetch — revocation latency is fetch-cadence, not release-cadence,
which is the whole argument for the external registry.

The rollback target comes from a per-package clean-version HISTORY the registry
repo maintains (each successful pipeline run appends the version it blessed). With
no history for an advised package, the script cannot auto-roll-back; it records
the advisory so CI opens an exception issue for the maintainer.

Only api.osv.dev is queried, via the Python standard library. Rollback selection
([`choose_rollback`]) is a PURE function of an `is_clean` callback, so
`--self-test` exercises it with no network; `--fixture DIR` and `--dry-run` run
the driver offline / without writing.

Usage:
    python3 rescan.py [--recipes PATH] [--history PATH] [--pins-out PATH]
                      [--report PATH] [--fixture DIR] [--dry-run] [--self-test]
"""

from __future__ import annotations

import argparse
import json
import sys
import tomllib
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from typing import Protocol

DEFAULT_RECIPES = Path("recipes.toml")
DEFAULT_HISTORY = Path("clean_history.toml")
USER_AGENT = "catenary-registry-rescan (+https://github.com/TwoWells/Catenary)"
TIMEOUT = 30

OSV_ECOSYSTEM = {"npm": "npm", "pip": "PyPI", "cargo": "crates.io", "go": "Go"}


class OsvSource(Protocol):
    """The single method [`rescan`] needs — satisfied by [`Fetcher`] and by test
    fakes alike (structural typing, so no test double must subclass `Fetcher`)."""

    def osv(self, ecosystem: str, package: str, version: str) -> list[str]: ...


class Fetcher:
    """OSV query layer — real HTTP, or canned replies under a fixture dir."""

    def __init__(self, fixture_dir: Path | None) -> None:
        self.fixture_dir = fixture_dir

    def osv(self, ecosystem: str, package: str, version: str) -> list[str]:
        """Advisory IDs affecting `package@version`, or [] if clean."""
        osv_eco = OSV_ECOSYSTEM.get(ecosystem)
        if osv_eco is None:
            return []
        body = {"version": version, "package": {"name": package, "ecosystem": osv_eco}}
        if self.fixture_dir is not None:
            return self._fixture(package, version)
        data = json.dumps(body).encode()
        req = urllib.request.Request(
            "https://api.osv.dev/v1/query",
            data=data,
            headers={"User-Agent": USER_AGENT, "Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:  # noqa: S310
            result = json.load(resp)
        return sorted(v.get("id", "") for v in result.get("vulns", []) or [])

    def _fixture(self, package: str, version: str) -> list[str]:
        """Read `<fixture_dir>/osv-<package>-<version>.json` → list of IDs."""
        base = self.fixture_dir
        if base is None:  # unreachable — callers guard on fixture_dir
            raise RuntimeError("fixture read requested without a fixture dir")
        slug = "".join(c if c.isalnum() else "-" for c in f"{package}-{version}")
        path = base / f"osv-{slug}.json"
        if not path.exists():
            return []
        return sorted(json.loads(path.read_text()))


def choose_rollback(current: str, history: list[str], is_clean) -> str | None:
    """Pick the newest known-clean predecessor to roll back to.

    `history` is the package's clean-version list in chronological (oldest-first)
    order. Walk it newest-first, skipping the current (advised) version, and return
    the first version `is_clean` still reports clean. Returns None when no clean
    predecessor exists (⇒ the maintainer must handle it).
    """
    for version in reversed(history):
        if version == current:
            continue
        if is_clean(version):
            return version
    return None


def rescan(recipes: dict[str, dict], history: dict[str, list[str]],
           fetch: OsvSource, now: datetime) -> dict:
    """Re-scan every pinned version; compute rollbacks for advisory hits."""
    hits: list[dict] = []
    rollbacks: list[dict] = []
    clean = 0
    for name in sorted(recipes):
        recipe = recipes[name]
        ecosystem = recipe.get("ecosystem", "")
        package = recipe.get("package", "")
        version = recipe.get("version", "")
        if not package or not version:
            continue
        try:
            advisories = fetch.osv(ecosystem, package, version)
        except (urllib.error.URLError, TimeoutError, OSError) as exc:
            hits.append({"server": name, "error": f"OSV query failed — {exc}"})
            continue
        if not advisories:
            clean += 1
            continue
        pkg_history = history.get(package, history.get(name, []))
        target = choose_rollback(
            version,
            pkg_history,
            lambda v: not fetch.osv(ecosystem, package, v),
        )
        entry = {
            "server": name,
            "ecosystem": ecosystem,
            "package": package,
            "pinned_version": version,
            "advisories": advisories,
            "rollback_to": target,
        }
        hits.append(entry)
        if target is not None:
            rollbacks.append(
                {"server": name, "from": version, "to": target, "advisories": advisories}
            )
    return {
        "generated_at": now.isoformat(),
        "scanned": len(recipes),
        "clean": clean,
        "advisory_hits": [h for h in hits if "advisories" in h],
        "errors": [h for h in hits if "error" in h],
        "rollbacks": rollbacks,
        "action_required": bool(hits),
    }


def apply_rollbacks(report: dict, path: Path) -> int:
    """Rewrite pinned versions to their rollback target, in place. Returns count.

    Note: the version is rolled back but the hash is left for the pipeline to
    re-resolve — a rollback to a known-clean predecessor still re-verifies bytes
    on the next pipeline run before republishing.
    """
    lines = path.read_text().splitlines(keepends=True) if path.exists() else []
    changed = 0
    for rb in report["rollbacks"]:
        header = f"[recipe.{rb['server']}]"
        start = next((i for i, ln in enumerate(lines) if ln.strip() == header), None)
        if start is None:
            continue
        end = len(lines)
        for i in range(start + 1, len(lines)):
            if lines[i].lstrip().startswith("["):
                end = i
                break
        for i in range(start, end):
            if lines[i].lstrip().startswith("version"):
                new = f'version = "{rb["to"]}"\n'
                if lines[i] != new:
                    lines[i] = new
                    changed += 1
                break
    if changed:
        path.write_text("".join(lines))
    return changed


def render_diff(report: dict) -> str:
    """A human-readable rollback diff for the exception issue / PR body."""
    if not report["advisory_hits"]:
        return "re-scan: all pinned versions clean.\n"
    out = ["re-scan found advisories on currently pinned versions:\n"]
    for hit in report["advisory_hits"]:
        ids = ", ".join(hit["advisories"])
        if hit["rollback_to"]:
            out.append(
                f"  {hit['server']}: {hit['pinned_version']} -> {hit['rollback_to']} "
                f"(rolled back; advisories: {ids})"
            )
        else:
            out.append(
                f"  {hit['server']}: {hit['pinned_version']} ADVISED ({ids}) — "
                f"NO clean predecessor in history; maintainer must intervene"
            )
    return "\n".join(out) + "\n"


def self_test() -> int:
    """Exercise the pure rollback chooser with no network."""
    history = ["1.0.0", "1.1.0", "1.2.0", "1.3.0"]
    # 1.3.0 (current) is advised; 1.2.0 is clean → roll back to 1.2.0.
    clean = {"1.0.0", "1.1.0", "1.2.0"}
    assert choose_rollback("1.3.0", history, lambda v: v in clean) == "1.2.0"
    # 1.2.0 also advised → fall further back to 1.1.0.
    clean2 = {"1.0.0", "1.1.0"}
    assert choose_rollback("1.3.0", history, lambda v: v in clean2) == "1.1.0"
    # Nothing clean → None (maintainer handles it).
    assert choose_rollback("1.3.0", history, lambda v: False) is None
    # No history → None.
    assert choose_rollback("1.3.0", [], lambda v: True) is None

    # The driver with a fixture-free fake fetcher.
    class FakeFetch:
        def osv(self, ecosystem, package, version):
            return ["GHSA-bad"] if version == "5.6.0" else []

    recipes = {
        "bash-ls": {"ecosystem": "npm", "package": "bash-language-server", "version": "5.6.0"},
        "taplo": {"ecosystem": "cargo", "package": "taplo-cli", "version": "0.10.0"},
    }
    hist = {"bash-language-server": ["5.4.0", "5.5.0", "5.6.0"]}
    report = rescan(recipes, hist, FakeFetch(), datetime(2026, 7, 7, tzinfo=timezone.utc))
    assert report["clean"] == 1, report
    assert report["action_required"] is True
    assert report["rollbacks"] == [
        {"server": "bash-ls", "from": "5.6.0", "to": "5.5.0", "advisories": ["GHSA-bad"]}
    ], report
    assert "5.6.0 -> 5.5.0" in render_diff(report)

    print("rescan self-test: OK", file=sys.stderr)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Daily OSV re-scan of pinned versions.")
    parser.add_argument("--recipes", type=Path, default=DEFAULT_RECIPES)
    parser.add_argument("--history", type=Path, default=DEFAULT_HISTORY)
    parser.add_argument("--pins-out", type=Path, default=None,
                        help="rewrite rolled-back pins into this recipes file")
    parser.add_argument("--report", type=Path, default=None)
    parser.add_argument("--fixture", type=Path, default=None)
    parser.add_argument("--dry-run", action="store_true", help="report only; never write pins")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    recipes = tomllib.loads(args.recipes.read_text()).get("recipe", {})
    history: dict[str, list[str]] = {}
    if args.history.exists():
        history = tomllib.loads(args.history.read_text()).get("clean", {})

    fetch = Fetcher(args.fixture)
    report = rescan(recipes, history, fetch, datetime.now(timezone.utc))

    print(json.dumps(report, indent=2))
    print(render_diff(report), file=sys.stderr)
    if args.report is not None:
        args.report.write_text(json.dumps(report, indent=2))

    if args.pins_out is not None and not args.dry_run and report["rollbacks"]:
        n = apply_rollbacks(report, args.pins_out)
        print(f"rolled back {n} pin(s) into {args.pins_out}", file=sys.stderr)

    # Non-zero when any advisory hit, so CI opens the rollback PR / exception issue.
    return 1 if report["action_required"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
