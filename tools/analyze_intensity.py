#!/usr/bin/env python3
"""Analyze intensity profiling data from profile_intensity test.

Reads the SQLite database written by tests/profile_intensity.rs and
produces per-server summaries answering the six questions from
SETTLE_DESIGN.md § "What profiling validates".

Usage: python3 tools/analyze_intensity.py [--db path/to/intensity.db]
"""

import argparse
import math
import sqlite3
import sys
from collections import defaultdict


DEFAULT_DB = "internal_repo/data/intensity_v2.db"

# Phase boundaries (ms) — overridable via --idle and --stimulus CLI args.
# Set by main() before any analysis runs.
IDLE_END = 20_000      # first 20s = idle baseline
STIMULUS_END = 30_000  # 20s-30s = stimulus


def connect(path):
    db = sqlite3.connect(path)
    db.row_factory = sqlite3.Row
    return db


def get_servers(db):
    rows = db.execute(
        "SELECT DISTINCT server FROM intensity_samples ORDER BY server"
    ).fetchall()
    return [r["server"] for r in rows]


def intensity(delta_pfc, delta_utime):
    """log(max(1, delta_pfc / delta_utime))"""
    if delta_utime == 0:
        return None
    return math.log(max(1.0, delta_pfc / delta_utime))


def intensity_combined(delta_pfc, delta_utime, delta_stime):
    """log(max(1, delta_pfc / (delta_utime + delta_stime)))"""
    denom = delta_utime + delta_stime
    if denom == 0:
        return None
    return math.log(max(1.0, delta_pfc / denom))


# ── Per-server analysis ──────────────────────────────────────────────


def analyze_server(db, server):
    """Analyze one server and return a dict of findings."""
    # Aggregate samples by timestamp (sum across process tree)
    rows = db.execute(
        """SELECT timestamp_ms,
                  SUM(delta_pfc) as pfc,
                  SUM(delta_utime) as utime,
                  SUM(delta_stime) as stime,
                  MAX(in_progress) as in_progress,
                  MAX(process_count) as process_count
           FROM intensity_samples
           WHERE server = ?
           GROUP BY timestamp_ms
           ORDER BY timestamp_ms""",
        (server,),
    ).fetchall()

    if not rows:
        return None

    # Normalize timestamps to start at 0 for this server
    t0 = rows[0]["timestamp_ms"]

    samples = []
    for r in rows:
        t = r["timestamp_ms"] - t0
        pfc = r["pfc"]
        ut = r["utime"]
        st = r["stime"]
        i_ut = intensity(pfc, ut)
        i_both = intensity_combined(pfc, ut, st)
        samples.append({
            "t": t,
            "pfc": pfc,
            "utime": ut,
            "stime": st,
            "intensity_ut": i_ut,
            "intensity_both": i_both,
            "in_progress": r["in_progress"],
            "process_count": r["process_count"],
        })

    # Classify phases
    idle = [s for s in samples if s["t"] < IDLE_END]
    stimulus = [s for s in samples if IDLE_END <= s["t"] < STIMULUS_END]
    recovery = [s for s in samples if s["t"] >= STIMULUS_END]

    # Idle baseline
    idle_pfcs = [s["pfc"] for s in idle]
    idle_nonzero = [p for p in idle_pfcs if p > 0]
    idle_intensities = [s["intensity_ut"] for s in idle if s["intensity_ut"] is not None]

    # Stimulus peak
    stim_intensities = [s["intensity_ut"] for s in stimulus if s["intensity_ut"] is not None]
    stim_pfcs = [s["pfc"] for s in stimulus]

    # Recovery tail — find when intensity last exceeds idle levels
    recovery_intensities = [
        (s["t"], s["intensity_ut"]) for s in recovery if s["intensity_ut"] is not None
    ]

    # Process tree
    proc_rows = db.execute(
        "SELECT DISTINCT pid, ppid FROM intensity_samples WHERE server = ?",
        (server,),
    ).fetchall()
    max_procs = max((s["process_count"] for s in samples), default=1)

    # Progress transitions
    progress_vals = [s["in_progress"] for s in samples]
    transitions = sum(
        1 for i in range(1, len(progress_vals)) if progress_vals[i] != progress_vals[i - 1]
    )

    # Find recovery settle time (last sample with nonzero intensity)
    settle_t = None
    for s in reversed(recovery):
        if s["pfc"] > 0:
            settle_t = s["t"]
            break

    # Find progress end time (last transition from >0 to 0)
    progress_end_t = None
    for i in range(1, len(samples)):
        if samples[i - 1]["in_progress"] > 0 and samples[i]["in_progress"] == 0:
            progress_end_t = samples[i]["t"]

    return {
        "total_samples": len(samples),
        "idle": {
            "count": len(idle),
            "mean_pfc": sum(idle_pfcs) / len(idle_pfcs) if idle_pfcs else 0,
            "max_pfc": max(idle_pfcs) if idle_pfcs else 0,
            "nonzero_pfc_count": len(idle_nonzero),
            "nonzero_pfc_frac": len(idle_nonzero) / len(idle_pfcs) if idle_pfcs else 0,
            "mean_intensity": (
                sum(idle_intensities) / len(idle_intensities) if idle_intensities else 0
            ),
            "max_intensity": max(idle_intensities) if idle_intensities else 0,
        },
        "stimulus": {
            "count": len(stimulus),
            "max_pfc": max(stim_pfcs) if stim_pfcs else 0,
            "max_intensity": max(stim_intensities) if stim_intensities else 0,
            "mean_intensity": (
                sum(stim_intensities) / len(stim_intensities) if stim_intensities else 0
            ),
            "nonzero_count": sum(1 for p in stim_pfcs if p > 0),
        },
        "recovery": {
            "count": len(recovery),
            "settle_t_ms": settle_t,
            "progress_end_t_ms": progress_end_t,
        },
        "tree": {
            "max_process_count": max_procs,
            "distinct_pids": len(proc_rows),
            "pids": [(r["pid"], r["ppid"]) for r in proc_rows],
        },
        "progress_transitions": transitions,
        "samples": samples,  # for detailed output
    }


# ── Output ───────────────────────────────────────────────────────────


def print_server(server, data):
    print(f"\n{'=' * 60}")
    print(f"  {server}")
    print(f"{'=' * 60}")

    idle = data["idle"]
    stim = data["stimulus"]
    rec = data["recovery"]
    tree = data["tree"]

    print(f"\n  Total samples: {data['total_samples']}")
    print(f"  Progress transitions: {data['progress_transitions']}")

    # Idle
    print(f"\n  ── Idle baseline (0–{IDLE_END // 1000}s) ──")
    print(f"  Samples: {idle['count']}")
    print(f"  Mean delta_pfc: {idle['mean_pfc']:.1f}   Max: {idle['max_pfc']}")
    print(f"  Nonzero pfc: {idle['nonzero_pfc_count']}/{idle['count']}"
          f" ({idle['nonzero_pfc_frac']:.0%})")
    print(f"  Mean intensity(utime): {idle['mean_intensity']:.3f}"
          f"   Max: {idle['max_intensity']:.3f}")

    # Stimulus
    print(f"\n  ── Stimulus ({IDLE_END // 1000}–{STIMULUS_END // 1000}s) ──")
    print(f"  Samples: {stim['count']}   Active (pfc>0): {stim['nonzero_count']}")
    print(f"  Max delta_pfc: {stim['max_pfc']}")
    print(f"  Max intensity(utime): {stim['max_intensity']:.3f}"
          f"   Mean: {stim['mean_intensity']:.3f}")

    # Recovery
    print(f"\n  ── Recovery ({STIMULUS_END // 1000}s+) ──")
    print(f"  Samples: {rec['count']}")
    if rec["settle_t_ms"] is not None:
        print(f"  Last nonzero pfc at: {rec['settle_t_ms']}ms"
              f" (+{rec['settle_t_ms'] - STIMULUS_END}ms into recovery)")
    else:
        print("  No activity in recovery phase")
    if rec["progress_end_t_ms"] is not None:
        print(f"  Last progress end at: {rec['progress_end_t_ms']}ms")
        if rec["settle_t_ms"] is not None:
            gap = rec["settle_t_ms"] - rec["progress_end_t_ms"]
            if gap > 0:
                print(f"  Intensity settled {gap}ms AFTER progress end")
            elif gap < 0:
                print(f"  Intensity settled {-gap}ms BEFORE progress end")
            else:
                print("  Intensity settled at same time as progress end")

    # Process tree
    print(f"\n  ── Process tree ──")
    print(f"  Max concurrent: {tree['max_process_count']}")
    print(f"  Distinct PIDs: {tree['distinct_pids']}")

    # Active samples detail (nonzero pfc)
    active = [s for s in data["samples"] if s["pfc"] > 0]
    if active:
        print(f"\n  ── Active samples (pfc > 0) ──")
        print(f"  {'time':>8}  {'pfc':>8}  {'utime':>6}  {'stime':>6}"
              f"  {'I(ut)':>7}  {'I(u+s)':>7}  {'procs':>5}  {'prog':>4}")
        for s in active:
            i_ut = f"{s['intensity_ut']:.2f}" if s["intensity_ut"] is not None else "skip"
            i_both = f"{s['intensity_both']:.2f}" if s["intensity_both"] is not None else "skip"
            print(f"  {s['t']:>7}ms  {s['pfc']:>8}  {s['utime']:>6}  {s['stime']:>6}"
                  f"  {i_ut:>7}  {i_both:>7}  {s['process_count']:>5}  {s['in_progress']:>4}")
    else:
        print("\n  No active samples (all pfc == 0)")


def print_cross_server(all_data):
    print(f"\n{'=' * 60}")
    print("  CROSS-SERVER COMPARISON")
    print(f"{'=' * 60}")

    # Q1: GC noise floor
    print("\n  ── Q1: GC noise floor ──")
    for server, data in sorted(all_data.items()):
        idle = data["idle"]
        runtime = classify_runtime(server)
        noise = "YES" if idle["nonzero_pfc_count"] > 0 else "no"
        print(f"  {server:>30} ({runtime:>6}): idle pfc noise={noise}"
              f"  mean={idle['mean_pfc']:.1f} max={idle['max_pfc']}")

    # Q2: Threshold sufficiency
    print("\n  ── Q2: Threshold sufficiency (delta_pfc > N) ──")
    for server, data in sorted(all_data.items()):
        idle_max = data["idle"]["max_pfc"]
        stim_max = data["stimulus"]["max_pfc"]
        if stim_max > 0:
            gap = stim_max / max(idle_max, 1)
            print(f"  {server:>30}: idle_max={idle_max:>6}  stim_max={stim_max:>8}"
                  f"  ratio={gap:>8.0f}x")
        else:
            print(f"  {server:>30}: no stimulus activity")

    # Q3: Bimodal distribution
    print("\n  ── Q3: Bimodal distribution ──")
    for server, data in sorted(all_data.items()):
        idle_max_i = data["idle"]["max_intensity"]
        stim_min_i = None
        active = [s for s in data["samples"] if s["pfc"] > 0 and s["intensity_ut"] is not None]
        if active:
            stim_min_i = min(s["intensity_ut"] for s in active)
        if stim_min_i is not None:
            gap = stim_min_i - idle_max_i
            verdict = "CLEAN" if gap > 1.0 else "NARROW" if gap > 0 else "OVERLAP"
            print(f"  {server:>30}: idle_max={idle_max_i:.2f}"
                  f"  active_min={stim_min_i:.2f}  gap={gap:.2f}  {verdict}")
        else:
            print(f"  {server:>30}: no active samples to compare")

    # Q4: Runtime family consistency
    print("\n  ── Q4: Runtime family consistency ──")
    families = defaultdict(list)
    for server, data in all_data.items():
        families[classify_runtime(server)].append((server, data))
    for runtime, members in sorted(families.items()):
        idle_maxes = [d["idle"]["max_pfc"] for _, d in members]
        stim_maxes = [d["stimulus"]["max_pfc"] for _, d in members]
        print(f"  {runtime}: {len(members)} servers"
              f"  idle_max_range=[{min(idle_maxes)}–{max(idle_maxes)}]"
              f"  stim_max_range=[{min(stim_maxes)}–{max(stim_maxes)}]")

    # Q5: Settle timing correlation
    print("\n  ── Q5: Settle timing vs progress ──")
    for server, data in sorted(all_data.items()):
        rec = data["recovery"]
        if rec["settle_t_ms"] is not None and rec["progress_end_t_ms"] is not None:
            gap = rec["settle_t_ms"] - rec["progress_end_t_ms"]
            print(f"  {server:>30}: intensity {'after' if gap > 0 else 'before'}"
                  f" progress by {abs(gap)}ms")
        elif rec["progress_end_t_ms"] is not None:
            print(f"  {server:>30}: progress ended, no recovery activity")
        else:
            print(f"  {server:>30}: no progress tokens observed")

    # Q6: Denominator selection
    print("\n  ── Q6: Denominator selection (utime vs utime+stime) ──")
    for server, data in sorted(all_data.items()):
        active = [s for s in data["samples"]
                  if s["pfc"] > 0
                  and s["intensity_ut"] is not None
                  and s["intensity_both"] is not None]
        if active:
            diffs = [abs(s["intensity_ut"] - s["intensity_both"]) for s in active]
            max_diff = max(diffs)
            mean_diff = sum(diffs) / len(diffs)
            print(f"  {server:>30}: max_diff={max_diff:.3f}  mean_diff={mean_diff:.3f}"
                  f"  ({'equivalent' if max_diff < 0.5 else 'DIVERGENT'})")
        else:
            print(f"  {server:>30}: no active samples")


def classify_runtime(server):
    if server in ("rust-analyzer", "taplo", "lattice"):
        return "Rust"
    if server == "clangd":
        return "C++"
    if server == "pyright-langserver":
        return "Python"
    if server in ("bash-language-server", "vscode-json-language-server",
                  "yaml-language-server"):
        return "Node"
    return "unknown"


# ── Main ─────────────────────────────────────────────────────────────


def main():
    global IDLE_END, STIMULUS_END
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", default=DEFAULT_DB, help="Path to intensity.db")
    parser.add_argument("--idle", type=int, default=IDLE_END,
                        help="Idle phase end (ms), default 20000")
    parser.add_argument("--stimulus", type=int, default=STIMULUS_END,
                        help="Stimulus phase end (ms), default 30000")
    args = parser.parse_args()
    IDLE_END = args.idle
    STIMULUS_END = args.stimulus

    db = connect(args.db)

    servers = get_servers(db)
    if not servers:
        print("No data in database.", file=sys.stderr)
        sys.exit(1)

    print(f"Servers: {', '.join(servers)}")

    all_data = {}
    for server in servers:
        data = analyze_server(db, server)
        if data:
            all_data[server] = data
            print_server(server, data)

    if len(all_data) > 1:
        print_cross_server(all_data)

    db.close()


if __name__ == "__main__":
    main()
