#!/usr/bin/env bash
# Reliable rebuild + run helper for the x86-64 emulator dev loop.
#
# Works around a flaky cargo change-detection in this environment: an `Edit` to a
# source file can leave its mtime <= the existing build artifact's (sub-second
# clock/granularity), so `cargo build` reports "Finished" without recompiling and
# silently runs a STALE binary. We bump the touched sources to a future mtime so
# cargo always recompiles, build, then reset mtimes to now.
#
# Usage:
#   scripts/dev-rebuild-run.sh <example-name> [timeout-secs] [-- <args...>]
# Example:
#   scripts/dev-rebuild-run.sh cc45_boot_dbg 120
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EX="${1:?usage: dev-rebuild-run.sh <example> [timeout] [-- args]}"
shift
TO=120
if [[ "${1:-}" =~ ^[0-9]+$ ]]; then TO="$1"; shift; fi
[[ "${1:-}" == "--" ]] && shift
ARGS=("$@")

cd "$ROOT"
# Force recompile: future mtime on the crate sources cargo is unreliable about.
find crates/holospaces/src -name '*.rs' -exec touch -d '+2 hours' {} +
touch -d '+2 hours' "crates/holospaces/examples/$EX.rs" 2>/dev/null || true
out="$(cargo build --release -p holospaces --example "$EX" 2>&1)"
rc=$?
find crates/holospaces/src -name '*.rs' -exec touch {} + 2>/dev/null
touch "crates/holospaces/examples/$EX.rs" 2>/dev/null || true
if [ $rc -ne 0 ]; then
    echo "$out" | grep -E "error" | head -20
    echo "BUILD FAILED ($rc)"; exit $rc
fi
echo "$out" | grep -qE "Compiling holospaces" && echo "[rebuilt holospaces]" || echo "[WARN: no recompile detected]"

bin="target/release/examples/$EX"
[ -x "$bin" ] || { echo "no binary $bin"; exit 1; }
echo "── running $EX (timeout ${TO}s) ──"
timeout "$TO" "$bin" "${ARGS[@]}"
echo "── exit $? ──"
