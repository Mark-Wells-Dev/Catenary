#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Mark Wells <contact@markwells.dev>
"""The gated auto-pinning pipeline (tui-rework 08).

STAGED, NOT LIVE. This script belongs in the *external registry repo* (which does
not exist yet — see `registry/README.md`). It PREPARES a pin bump behind
mechanical gates and emits a machine-readable gate report; it never merges.
Merging is the registry repo's CI policy: all-green ⇒ auto-merge + publish; any
red ⇒ an issue for the maintainer, who is an exception handler, not a reviewer of
hash diffs.

For each `[recipe.<server>]` in the registry's `recipes.toml` (the same schema
Catenary ships in `defaults/recipes.toml`), the pipeline:

  1. resolves the latest candidate version per ecosystem (npm / PyPI / crates.io /
     Go module proxy), and
  2. runs it through the gates, in order:

     - COOLING-OFF   the candidate's publish timestamp is >= N days old (default
                     10, per-ecosystem tunable). The highest-value defence against
                     the npm-wave class — malicious versions are typically yanked
                     within hours-to-days. HARD gate (a fail blocks).
     - OSV ADVISORY  api.osv.dev reports no advisory for the exact
                     package@version. HARD gate.
     - PROVENANCE    Sigstore-backed build provenance / attestation, where the
                     ecosystem offers it (npm provenance today). Recorded
                     has/hasn't HONESTLY; a "flag" by default (non-blocking),
                     escalated to a hard gate with --require-provenance.
     - ANOMALY       major-version jump beyond a bound, artifact-size delta beyond
                     a bound, and maintainer-set change — each a flag that routes
                     the package to the exception handler. `n/a` where the
                     ecosystem does not expose the datum (recorded honestly).

  3. emits the updated pins (green packages only) and the gate report.

The conformance gate (07's matrix on the bumped bytes) is NOT run here — it is a
separate `workflow_dispatch` back against this repo's conformance matrix (see
`registry/workflows/conformance-dispatch.yml`); this script records the intent
and the report is the artifact CI feeds into that dispatch.

Only the four registry HTTP metadata APIs + api.osv.dev are used, via the Python
standard library — no npm/cargo/pip/go CLI, no curl/jq, no third-party package.

Offline / determinism: gate evaluation ([`evaluate_gates`]) is a PURE function of
already-gathered facts, so `--self-test` exercises every gate outcome with
synthetic facts and no network. `--fixture DIR` reads canned metadata JSON from a
directory instead of the network, and `--dry-run` resolves + reports without
writing pins.

Usage:
    python3 gate_pipeline.py [--recipes PATH] [--report PATH] [--pins-out PATH]
                             [--cooling-days N] [--only NAME ...]
                             [--max-major-jump N] [--max-size-delta FRACTION]
                             [--require-provenance] [--dry-run]
                             [--fixture DIR] [--self-test]
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

DEFAULT_RECIPES = Path("recipes.toml")
USER_AGENT = "catenary-registry-gate (+https://github.com/TwoWells/Catenary)"
TIMEOUT = 30

# Default gate thresholds (all CLI-overridable).
DEFAULT_COOLING_DAYS = 10
# Per-ecosystem cooling-off overrides (a slower-moving ecosystem may want longer).
COOLING_DAYS_BY_ECOSYSTEM: dict[str, int] = {
    # e.g. "npm": 10, "pip": 10, "cargo": 7, "go": 7 — tune on sightings.
}
DEFAULT_MAX_MAJOR_JUMP = 1
DEFAULT_MAX_SIZE_DELTA = 0.5  # 50% artifact-size swing flags for review.

# Which ecosystems expose which datum today (honest availability, not aspiration).
PROVENANCE_ECOSYSTEMS = {"npm"}  # npm publishes Sigstore provenance/attestations.
SIZE_ECOSYSTEMS = {"npm"}  # npm registry metadata carries dist.unpackedSize.
MAINTAINER_ECOSYSTEMS = {"npm", "cargo"}  # npm maintainers; crates.io owners.

# OSV ecosystem names (api.osv.dev) keyed by our recipe ecosystem token.
OSV_ECOSYSTEM = {"npm": "npm", "pip": "PyPI", "cargo": "crates.io", "go": "Go"}


# ── network layer (injectable via --fixture) ─────────────────────────────


class Fetcher:
    """Fetches registry/OSV metadata. Real HTTP, or canned files under a fixture
    dir when one is supplied (so the pipeline runs deterministically offline)."""

    def __init__(self, fixture_dir: Path | None) -> None:
        self.fixture_dir = fixture_dir

    def get_json(self, url: str) -> dict:
        """GET `url` and parse JSON (or read the fixture file for it)."""
        if self.fixture_dir is not None:
            return self._fixture(url)
        req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:  # noqa: S310
            return json.load(resp)

    def post_json(self, url: str, body: dict) -> dict:
        """POST `body` as JSON to `url` and parse the JSON reply (OSV query)."""
        if self.fixture_dir is not None:
            return self._fixture(url, body)
        data = json.dumps(body).encode()
        req = urllib.request.Request(
            url,
            data=data,
            headers={"User-Agent": USER_AGENT, "Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=TIMEOUT) as resp:  # noqa: S310
            return json.load(resp)

    def _fixture(self, url: str, body: dict | None = None) -> dict:
        """Read `<fixture_dir>/<slug>.json` for a URL (+ optional POST body)."""
        base = self.fixture_dir
        if base is None:  # unreachable — callers guard on fixture_dir
            raise RuntimeError("fixture read requested without a fixture dir")
        slug = _slug(url if body is None else f"{url}|{json.dumps(body, sort_keys=True)}")
        path = base / f"{slug}.json"
        if not path.exists():
            raise FileNotFoundError(f"no fixture for {url} (expected {path})")
        return json.loads(path.read_text())


def _slug(text: str) -> str:
    """A filesystem-safe slug for a URL, matching a fixture file name."""
    return "".join(c if c.isalnum() else "-" for c in text).strip("-")[:200]


# ── resolvers: latest candidate + the facts each gate needs ──────────────


def _iso(ts: str) -> datetime:
    """Parse an ISO-8601 timestamp (tolerating a trailing `Z`) as UTC-aware."""
    return datetime.fromisoformat(ts.replace("Z", "+00:00"))


def gather_npm(package: str, fetch: Fetcher) -> dict:
    """Candidate facts for an npm package: latest version, publish time, size,
    maintainers, and whether the version carries provenance attestations."""
    latest = fetch.get_json(f"https://registry.npmjs.org/{package}/latest")
    version = latest["version"]
    full = fetch.get_json(f"https://registry.npmjs.org/{package}")
    published = full.get("time", {}).get(version)
    meta = full.get("versions", {}).get(version, {})
    dist = meta.get("dist", {})
    return {
        "version": version,
        "published": published,
        "size": dist.get("unpackedSize"),
        "maintainers": sorted(m.get("name", "") for m in meta.get("maintainers", [])),
        # npm records Sigstore provenance under dist.attestations (a URL) or the
        # provenance flag on the version's `dist`. Present ⇒ attested.
        "provenance": bool(dist.get("attestations") or dist.get("signatures")),
        "integrity": dist.get("integrity"),
    }


def gather_pip(package: str, fetch: Fetcher) -> dict:
    """Candidate facts for a PyPI package. Provenance (PEP 740 attestations) and
    a stable maintainer set are not reliably in the JSON API, so they are recorded
    n/a rather than guessed."""
    data = fetch.get_json(f"https://pypi.org/pypi/{package}/json")
    version = data["info"]["version"]
    files = data.get("releases", {}).get(version, [])
    published = files[0].get("upload_time_iso_8601") if files else None
    size = files[0].get("size") if files else None
    return {
        "version": version,
        "published": published,
        "size": size,
        "maintainers": None,  # not exposed reliably → n/a
        "provenance": None,  # PEP 740 attestations not queried here → n/a
        "integrity": None,
    }


def gather_cargo(package: str, fetch: Fetcher) -> dict:
    """Candidate facts for a crates.io crate: latest stable + created_at; owners
    for the maintainer-change gate."""
    data = fetch.get_json(f"https://crates.io/api/v1/crates/{package}")
    version = data["crate"]["max_stable_version"]
    published = None
    size = None
    for v in data.get("versions", []):
        if v.get("num") == version:
            published = v.get("created_at")
            size = v.get("crate_size")
            break
    owners = fetch.get_json(f"https://crates.io/api/v1/crates/{package}/owners")
    maintainers = sorted(o.get("login", "") for o in owners.get("users", []))
    return {
        "version": version,
        "published": published,
        "size": size,
        "maintainers": maintainers,
        "provenance": None,  # crates.io offers no Sigstore provenance → n/a
        "integrity": None,
    }


def _go_escape(module: str) -> str:
    """Go module-proxy case-encoding: an uppercase letter becomes `!<lower>`."""
    return "".join(f"!{c.lower()}" if c.isupper() else c for c in module)


def gather_go(module: str, fetch: Fetcher) -> dict:
    """Candidate facts for a Go module: latest version + publish Time from the
    proxy `@latest` info. Provenance/size/maintainers are not exposed (the
    checksum DB provides transparency, not Sigstore provenance) → n/a."""
    esc = _go_escape(module)
    info = fetch.get_json(f"https://proxy.golang.org/{esc}/@latest")
    return {
        "version": info["Version"],
        "published": info.get("Time"),
        "size": None,
        "maintainers": None,
        "provenance": None,
        "integrity": None,
    }


GATHERERS = {
    "npm": gather_npm,
    "pip": gather_pip,
    "cargo": gather_cargo,
    "go": gather_go,
}


def query_osv(ecosystem: str, package: str, version: str, fetch: Fetcher) -> list[str]:
    """Return OSV advisory IDs affecting `package@version`, or [] if clean."""
    osv_eco = OSV_ECOSYSTEM.get(ecosystem)
    if osv_eco is None:
        return []
    body = {"version": version, "package": {"name": package, "ecosystem": osv_eco}}
    result = fetch.post_json("https://api.osv.dev/v1/query", body)
    return sorted(v.get("id", "") for v in result.get("vulns", []) or [])


# ── version helpers ──────────────────────────────────────────────────────


def major_of(version: str) -> int | None:
    """The integer major component of a version (tolerating a `v` prefix), or
    None when it cannot be parsed."""
    core = version.lstrip("vV").split("+", 1)[0].split("-", 1)[0]
    head = core.split(".", 1)[0]
    return int(head) if head.isdigit() else None


# ── the gates (PURE — no network, so --self-test covers every outcome) ────


def gate_cooling_off(facts: dict, min_days: int, now: datetime) -> dict:
    """Publish timestamp must be at least `min_days` old. HARD (a fail blocks)."""
    published = facts.get("published")
    if not published:
        return _gate("fail", "no publish timestamp available", blocking=True)
    age_days = (now - _iso(published)).total_seconds() / 86400.0
    ok = age_days >= min_days
    return _gate(
        "pass" if ok else "fail",
        f"published {age_days:.1f}d ago (min {min_days}d)",
        blocking=True,
        age_days=round(age_days, 1),
        min_days=min_days,
    )


def gate_osv(advisories: list[str]) -> dict:
    """No OSV advisory may affect the candidate. HARD (a fail blocks)."""
    if advisories:
        return _gate("fail", f"OSV advisories: {', '.join(advisories)}", blocking=True,
                     advisories=advisories)
    return _gate("pass", "no OSV advisory", blocking=True, advisories=[])


def gate_provenance(facts: dict, ecosystem: str, require: bool) -> dict:
    """Record whether the candidate carries build provenance, honestly.

    - ecosystem offers provenance + present ⇒ pass;
    - ecosystem offers provenance + absent  ⇒ flag (blocks only with --require);
    - ecosystem offers no provenance        ⇒ n/a (never blocks).
    """
    if ecosystem not in PROVENANCE_ECOSYSTEMS or facts.get("provenance") is None:
        return _gate("n/a", f"{ecosystem} exposes no build-provenance attestation")
    if facts["provenance"]:
        return _gate("pass", "Sigstore provenance attestation present")
    return _gate(
        "fail" if require else "flag",
        "no provenance attestation (--require-provenance would block)",
        blocking=require,
    )


def gate_major_jump(candidate: str, current: str, max_jump: int) -> dict:
    """Flag a major-version jump beyond `max_jump` (blocks — a 1.x→5.x leap is
    a takeover signature, not a normal bump)."""
    cand, cur = major_of(candidate), major_of(current)
    if cand is None or cur is None:
        return _gate("n/a", f"unparseable major ({current} -> {candidate})")
    jump = cand - cur
    ok = jump <= max_jump
    return _gate(
        "pass" if ok else "fail",
        f"major {cur} -> {cand} (jump {jump}, max {max_jump})",
        blocking=True,
        jump=jump,
    )


def gate_size_delta(facts: dict, ecosystem: str, current_size: int | None,
                    max_delta: float) -> dict:
    """Flag an artifact-size swing beyond `max_delta` (fraction). n/a where the
    ecosystem does not expose a size or no previous size is recorded."""
    new_size = facts.get("size")
    if ecosystem not in SIZE_ECOSYSTEMS or new_size is None or not current_size:
        return _gate("n/a", "artifact size not available for this ecosystem")
    delta = abs(new_size - current_size) / current_size
    ok = delta <= max_delta
    return _gate(
        "pass" if ok else "fail",
        f"size {current_size} -> {new_size} (Δ {delta:.0%}, max {max_delta:.0%})",
        blocking=True,
        delta=round(delta, 3),
    )


def gate_maintainer_change(facts: dict, ecosystem: str,
                           previous: list[str] | None) -> dict:
    """Flag a change in the maintainer/owner set (blocks — an added publisher is
    a supply-chain signal). n/a where the ecosystem does not expose maintainers,
    or when no previous set is on record."""
    current = facts.get("maintainers")
    if ecosystem not in MAINTAINER_ECOSYSTEMS or current is None or previous is None:
        return _gate("n/a", "maintainer set not available for this ecosystem")
    if sorted(current) == sorted(previous):
        return _gate("pass", "maintainer set unchanged", blocking=True)
    added = sorted(set(current) - set(previous))
    removed = sorted(set(previous) - set(current))
    return _gate(
        "fail",
        f"maintainer set changed (added {added}, removed {removed})",
        blocking=True,
        added=added,
        removed=removed,
    )


def _gate(status: str, detail: str, *, blocking: bool = False, **extra) -> dict:
    """Build one gate result. `blocking` marks a `fail` that vetoes the merge."""
    return {"status": status, "blocking": blocking, "detail": detail, **extra}


def evaluate_gates(
    facts: dict,
    recipe: dict,
    *,
    min_days: int,
    max_major_jump: int,
    max_size_delta: float,
    require_provenance: bool,
    osv_advisories: list[str],
    now: datetime,
    previous_maintainers: list[str] | None = None,
    previous_size: int | None = None,
) -> dict:
    """Run every gate over already-gathered facts. Pure — no I/O — so tests and
    --self-test drive every outcome deterministically."""
    ecosystem = recipe.get("ecosystem", "")
    current_version = recipe.get("version", "")
    gates = {
        "cooling_off": gate_cooling_off(facts, min_days, now),
        "osv": gate_osv(osv_advisories),
        "provenance": gate_provenance(facts, ecosystem, require_provenance),
        "anomaly_major_jump": gate_major_jump(
            facts.get("version", ""), current_version, max_major_jump
        ),
        "anomaly_size_delta": gate_size_delta(
            facts, ecosystem, previous_size, max_size_delta
        ),
        "anomaly_maintainer_change": gate_maintainer_change(
            facts, ecosystem, previous_maintainers
        ),
    }
    # Mergeable ⇔ no gate is a BLOCKING fail. `flag`/`n/a` never block on their
    # own; `flag` (unattested provenance) blocks only under --require-provenance,
    # which flips it to a blocking `fail`.
    mergeable = not any(
        g["status"] == "fail" and g["blocking"] for g in gates.values()
    )
    return {
        "server": recipe.get("_server", ""),
        "ecosystem": ecosystem,
        "package": recipe.get("package", ""),
        "current_version": current_version,
        "candidate_version": facts.get("version", ""),
        "gates": gates,
        "mergeable": mergeable,
    }


# ── driver ───────────────────────────────────────────────────────────────


def cooling_days_for(ecosystem: str, default: int) -> int:
    """The cooling-off horizon for an ecosystem (per-ecosystem override wins)."""
    return COOLING_DAYS_BY_ECOSYSTEM.get(ecosystem, default)


def run_pipeline(recipes: dict[str, dict], fetch: Fetcher, args,
                 now: datetime) -> dict:
    """Resolve + gate every selected recipe, returning the full gate report."""
    selected = args.only if args.only else sorted(recipes)
    packages: list[dict] = []
    for name in selected:
        recipe = recipes.get(name)
        if recipe is None:
            packages.append({"server": name, "error": "not found in recipes"})
            continue
        recipe = {**recipe, "_server": name}
        ecosystem = recipe.get("ecosystem", "")
        package = recipe.get("package", "")
        gather = GATHERERS.get(ecosystem)
        if gather is None or not package:
            packages.append(
                {"server": name, "error": f"unknown ecosystem/package ({ecosystem}/{package})"}
            )
            continue
        try:
            facts = gather(package, fetch)
            advisories = query_osv(ecosystem, package, facts["version"], fetch)
        except (urllib.error.URLError, TimeoutError, KeyError, OSError,
                FileNotFoundError) as exc:
            packages.append({"server": name, "error": f"resolve failed — {exc}"})
            continue
        report = evaluate_gates(
            facts,
            recipe,
            min_days=cooling_days_for(ecosystem, args.cooling_days),
            max_major_jump=args.max_major_jump,
            max_size_delta=args.max_size_delta,
            require_provenance=args.require_provenance,
            osv_advisories=advisories,
            now=now,
            # A registry repo records the previous maintainer set + size alongside
            # each pin; absent (first run) ⇒ the anomaly gates report n/a.
            previous_maintainers=recipe.get("_maintainers"),
            previous_size=recipe.get("_size"),
        )
        report["hash"] = facts.get("integrity")
        packages.append(report)

    mergeable = [p for p in packages if p.get("mergeable")]
    return {
        "generated_at": now.isoformat(),
        "cooling_days_default": args.cooling_days,
        "mergeable": all(p.get("mergeable") for p in packages if "error" not in p)
        and not any("error" in p for p in packages),
        "mergeable_count": len(mergeable),
        "package_count": len(packages),
        "packages": packages,
    }


def write_pins(recipes: dict[str, dict], report: dict, path: Path) -> int:
    """Rewrite `path`'s version (and npm hash) pins for the GREEN packages only,
    editing lines in place so comments and layout survive. Returns the count."""
    lines = path.read_text().splitlines(keepends=True) if path.exists() else []
    changed = 0
    for pkg in report["packages"]:
        if not pkg.get("mergeable"):
            continue
        name = pkg["server"]
        version = pkg["candidate_version"]
        hash_ = pkg.get("hash")
        if _edit_pin(lines, name, version, hash_):
            changed += 1
    if changed:
        path.write_text("".join(lines))
    return changed


def _edit_pin(lines: list[str], name: str, version: str, hash_: str | None) -> bool:
    """Rewrite one `[recipe.<name>]` block's version/hash lines. Mirrors
    `tools/refresh_recipes.py::edit_pin` so the two stay behaviourally identical."""
    header = f"[recipe.{name}]"
    start = next((i for i, ln in enumerate(lines) if ln.strip() == header), None)
    if start is None:
        return False
    end = len(lines)
    for i in range(start + 1, len(lines)):
        if lines[i].lstrip().startswith("["):
            end = i
            break
    changed = False
    saw_hash = False
    for i in range(start, end):
        stripped = lines[i].lstrip()
        if stripped.startswith("version"):
            new = f'version = "{version}"\n'
            if lines[i] != new:
                lines[i] = new
                changed = True
        elif stripped.startswith("hash") and hash_:
            saw_hash = True
            new = f'hash = "{hash_}"\n'
            if lines[i] != new:
                lines[i] = new
                changed = True
    if hash_ and not saw_hash:
        for i in range(start, end):
            if lines[i].lstrip().startswith("version"):
                lines.insert(i + 1, f'hash = "{hash_}"\n')
                changed = True
                break
    return changed


# ── --self-test: every gate outcome, no network ──────────────────────────


def self_test() -> int:
    """Assert each gate's pass/fail/flag/n/a outcomes with synthetic facts."""
    now = datetime(2026, 7, 7, tzinfo=timezone.utc)
    fresh = "2026-07-06T00:00:00Z"  # 1 day old
    cooled = "2026-06-01T00:00:00Z"  # >30 days old

    assert gate_cooling_off({"published": fresh}, 10, now)["status"] == "fail"
    assert gate_cooling_off({"published": cooled}, 10, now)["status"] == "pass"
    assert gate_cooling_off({}, 10, now)["status"] == "fail"

    assert gate_osv(["GHSA-x"])["status"] == "fail"
    assert gate_osv([])["status"] == "pass"

    assert gate_provenance({"provenance": True}, "npm", False)["status"] == "pass"
    assert gate_provenance({"provenance": False}, "npm", False)["status"] == "flag"
    assert gate_provenance({"provenance": False}, "npm", True)["status"] == "fail"
    assert gate_provenance({"provenance": None}, "cargo", False)["status"] == "n/a"

    assert gate_major_jump("1.2.0", "1.1.0", 1)["status"] == "pass"
    assert gate_major_jump("2.0.0", "1.1.0", 1)["status"] == "pass"
    assert gate_major_jump("5.0.0", "1.1.0", 1)["status"] == "fail"
    assert gate_major_jump("v0.23.0", "v0.22.0", 1)["status"] == "pass"

    assert gate_size_delta({"size": 1000}, "npm", 900, 0.5)["status"] == "pass"
    assert gate_size_delta({"size": 5000}, "npm", 900, 0.5)["status"] == "fail"
    assert gate_size_delta({"size": None}, "npm", 900, 0.5)["status"] == "n/a"
    assert gate_size_delta({"size": 1000}, "go", 900, 0.5)["status"] == "n/a"

    same = ["alice", "bob"]
    assert gate_maintainer_change({"maintainers": same}, "npm", same)["status"] == "pass"
    assert (
        gate_maintainer_change({"maintainers": ["alice", "mallory"]}, "npm", same)[
            "status"
        ]
        == "fail"
    )
    assert gate_maintainer_change({"maintainers": None}, "go", None)["status"] == "n/a"

    # An all-green candidate is mergeable; a single blocking fail is not.
    recipe = {"ecosystem": "npm", "package": "x", "version": "1.0.0", "_server": "x"}
    green = evaluate_gates(
        {"version": "1.1.0", "published": cooled, "size": 950, "provenance": True,
         "maintainers": same},
        recipe, min_days=10, max_major_jump=1, max_size_delta=0.5,
        require_provenance=False, osv_advisories=[], now=now,
        previous_maintainers=same, previous_size=1000,
    )
    assert green["mergeable"] is True, green
    red = evaluate_gates(
        {"version": "1.1.0", "published": fresh, "size": 950, "provenance": True,
         "maintainers": same},
        recipe, min_days=10, max_major_jump=1, max_size_delta=0.5,
        require_provenance=False, osv_advisories=[], now=now,
        previous_maintainers=same, previous_size=1000,
    )
    assert red["mergeable"] is False, red

    print("gate_pipeline self-test: OK", file=sys.stderr)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Gated auto-pinning pipeline.")
    parser.add_argument("--recipes", type=Path, default=DEFAULT_RECIPES)
    parser.add_argument("--report", type=Path, default=None, help="write the JSON report here")
    parser.add_argument("--pins-out", type=Path, default=None,
                        help="rewrite pins for green packages into this recipes file")
    parser.add_argument("--cooling-days", type=int, default=DEFAULT_COOLING_DAYS)
    parser.add_argument("--max-major-jump", type=int, default=DEFAULT_MAX_MAJOR_JUMP)
    parser.add_argument("--max-size-delta", type=float, default=DEFAULT_MAX_SIZE_DELTA)
    parser.add_argument("--require-provenance", action="store_true")
    parser.add_argument("--only", nargs="*", default=None)
    parser.add_argument("--dry-run", action="store_true", help="report only; never write pins")
    parser.add_argument("--fixture", type=Path, default=None,
                        help="read metadata from canned JSON here instead of the network")
    parser.add_argument("--self-test", action="store_true", help="run gate assertions, no network")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    recipes = tomllib.loads(args.recipes.read_text()).get("recipe", {})
    fetch = Fetcher(args.fixture)
    now = datetime.now(timezone.utc)
    report = run_pipeline(recipes, fetch, args, now)

    print(json.dumps(report, indent=2))
    print(
        f"gate report: {report['mergeable_count']}/{report['package_count']} "
        f"package(s) mergeable, overall {'GREEN' if report['mergeable'] else 'RED'}",
        file=sys.stderr,
    )
    if args.report is not None:
        args.report.write_text(json.dumps(report, indent=2))

    if args.pins_out is not None and not args.dry_run:
        n = write_pins(recipes, report, args.pins_out)
        print(f"rewrote {n} green pin(s) into {args.pins_out}", file=sys.stderr)

    # Exit non-zero when anything is un-mergeable, so CI opens the exception issue.
    return 0 if report["mergeable"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
