#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (C) 2026 Mark Wells <contact@markwells.dev>
#
# grep_rename_race_probe.sh — regression probe for bugs 34/35 (walk hardening).
#
# Reproduces the atomic-rename write race that made `catenary grep` return
# "0 matches in 0 files" for a file that is present on disk (bug 34) and the
# cold first-grep under-return that self-heals on retry (bug 35).
#
# Two cases:
#   - SEQUENTIAL (in-workflow): write a file via atomic rename (write temp +
#     rename), then grep it sequentially. This is the agent's real workflow.
#     ACCEPTANCE: misses must be 0.
#   - CONCURRENT (saturating hammer): background writers churn atomic renames
#     while greps run. A residual here is acceptable — it is the documented
#     concurrent-writer liveness non-goal, not the in-workflow contract.
#
# Each case runs against an isolated daemon (its own XDG_STATE_HOME and roots),
# never the user's production daemon.
#
# Usage: tools/grep_rename_race_probe.sh [SEQ_ITERS] [CONC_ITERS]
set -euo pipefail

SEQ_ITERS="${1:-200}"
CONC_ITERS="${2:-200}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

echo "building catenary (debug) ..." >&2
cargo build --bin catenary >&2
BIN="$REPO_ROOT/target/debug/catenary"
if [[ ! -x "$BIN" ]]; then
	echo "FATAL: catenary binary not found at $BIN" >&2
	exit 2
fi

WORK="$(mktemp -d)"
trap 'cleanup' EXIT

DAEMON_PID=""

cleanup() {
	if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
		kill "$DAEMON_PID" 2>/dev/null || true
		wait "$DAEMON_PID" 2>/dev/null || true
	fi
	rm -rf "$WORK"
}

# Isolated XDG bases so the probe never touches the user's daemon/state.
export XDG_STATE_HOME="$WORK/state"
export XDG_CONFIG_HOME="$WORK/config"
export XDG_DATA_HOME="$WORK/data"
export XDG_RUNTIME_DIR="$WORK/runtime"
export XDG_CACHE_HOME="$WORK/cache"
mkdir -p "$XDG_STATE_HOME" "$XDG_CONFIG_HOME" "$XDG_DATA_HOME" "$XDG_RUNTIME_DIR" "$XDG_CACHE_HOME"
# Clear any inherited daemon config that could redirect search behaviour.
unset CATENARY_SERVERS CATENARY_CONFIG CATENARY_LOG 2>/dev/null || true

ROOT="$WORK/root"
mkdir -p "$ROOT"
SOCK="$XDG_STATE_HOME/catenary/catenary.sock"

echo "starting isolated daemon (root=$ROOT) ..." >&2
CATENARY_ROOTS="$ROOT" "$BIN" daemon >"$WORK/daemon.log" 2>&1 &
DAEMON_PID=$!

# Wait for the IPC socket.
for _ in $(seq 1 100); do
	[[ -S "$SOCK" ]] && break
	if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
		echo "FATAL: daemon exited early; log:" >&2
		cat "$WORK/daemon.log" >&2
		exit 2
	fi
	sleep 0.1
done
if [[ ! -S "$SOCK" ]]; then
	echo "FATAL: daemon socket never appeared at $SOCK" >&2
	cat "$WORK/daemon.log" >&2
	exit 2
fi

# Writes content to $1 via an atomic rename (write temp in same dir + rename).
atomic_write() {
	local target="$1" content="$2"
	local tmp
	tmp="$(dirname "$target")/.$(basename "$target").tmp.$$"
	printf '%s\n' "$content" >"$tmp"
	mv -f "$tmp" "$target"
}

# Greps $2 (a named path) for $1 and returns 0 iff the needle is found.
grep_hits() {
	local needle="$1" path="$2"
	"$BIN" grep "$needle" "$path" 2>/dev/null | grep -q "$needle"
}

# ── Sequential (in-workflow) case ─────────────────────────────────────
seq_miss=0
for i in $(seq 1 "$SEQ_ITERS"); do
	needle="seq_needle_${i}"
	target="$ROOT/seq_${i}.rs"
	atomic_write "$target" "let ${needle} = 1;"
	if ! grep_hits "$needle" "$target"; then
		seq_miss=$((seq_miss + 1))
	fi
done
echo "SEQUENTIAL: ${seq_miss}/${SEQ_ITERS} misses"

# ── Concurrent (saturating hammer) case ───────────────────────────────
CTARGET="$ROOT/concurrent.rs"
atomic_write "$CTARGET" "let conc_needle = 1;"
HAMMER_STOP="$WORK/hammer.stop"
rm -f "$HAMMER_STOP"

# Background writers churn the same path via atomic rename, saturating the
# rename window the reader races against.
(
	n=0
	while [[ ! -e "$HAMMER_STOP" ]]; do
		atomic_write "$CTARGET" "let conc_needle = $((n++));"
	done
) &
HAMMER_PID=$!

conc_miss=0
for _ in $(seq 1 "$CONC_ITERS"); do
	if ! grep_hits "conc_needle" "$CTARGET"; then
		conc_miss=$((conc_miss + 1))
	fi
done
touch "$HAMMER_STOP"
wait "$HAMMER_PID" 2>/dev/null || true
echo "CONCURRENT: ${conc_miss}/${CONC_ITERS} misses (residual acceptable — concurrent-writer non-goal)"

# Acceptance: sequential must be 0. Concurrent residual is acceptable.
if [[ "$seq_miss" -ne 0 ]]; then
	echo "FAIL: sequential in-workflow case must be 0 misses (got ${seq_miss})" >&2
	exit 1
fi
echo "PASS: sequential in-workflow case is 0 misses"
exit 0
