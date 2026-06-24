#!/usr/bin/env bash
# Robust boot-diagnostic runner for the x86-64 emulator — maintains the dev
# environment so we never debug a STALE binary or write to a volatile path.
#
# Two environment hazards this guards against:
#  1. STALE BINARIES. cargo's change-detection here is unreliable: an edited
#     source can keep an mtime <= its build artifact (coarse/again-set mtimes), so
#     `cargo build` prints "Finished" WITHOUT recompiling and silently runs the
#     OLD binary. We bump every crate source to a FUTURE mtime so cargo must
#     recompile, then assert "Compiling holospaces" actually appeared — aborting
#     loudly if not, rather than letting a stale run mislead us.
#  2. VOLATILE /tmp. /tmp is tmpfs and is wiped under us; the diag example writes
#     to a persistent worktree file (boot-diag.log), and we read THAT.
#
# Usage: scripts/dev-boot.sh [slice_count]   (default 200 slices = 20B cycles)
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
SLICES="${1:-200}"
EX=cc45_boot_diag
LOG="$ROOT/boot-diag.log"

# Force a real recompile (defeat the mtime bug), then verify it happened.
find crates/holospaces/src crates/holospaces/examples -name '*.rs' -exec touch -d '+2 hours' {} +
build_out="$(cargo build --release -q -p holospaces --example "$EX" 2>&1)"
rc=$?
find crates/holospaces/src crates/holospaces/examples -name '*.rs' -exec touch {} +
if [ $rc -ne 0 ]; then
    echo "$build_out" | grep -E "error" | head -20
    echo "BUILD FAILED"; exit $rc
fi
if ! echo "$build_out" | grep -q "Compiling holospaces"; then
    echo "WARNING: holospaces did not recompile — the binary may be STALE."
    echo "         (mtime change-detection failed; refusing to trust the run.)"
    exit 2
fi

BIN="$ROOT/target/release/examples/$EX"
[ -x "$BIN" ] || { echo "missing $BIN"; exit 1; }
echo "[fresh build: $BIN  $(stat -c %y "$BIN" | cut -d. -f1)]"

# Stop any prior run so we never read another run's log.
pkill -9 -f "$EX" 2>/dev/null
rm -f "$LOG"
nohup "$BIN" "$SLICES" >/dev/null 2>&1 &
pid=$!
echo "[booting pid $pid -> $LOG]  (tail it; Ctrl-C is safe)"
# Stream the persistent log until the boot halts or the budget is exhausted.
until grep -qaE "=== (HALT|budget)" "$LOG" 2>/dev/null || ! kill -0 "$pid" 2>/dev/null; do
    sleep 4
done
echo "=== boot-diag summary ==="
grep -aE "CC45-|=== HALT|=== budget|prot=\+[1-9]|nopage=\+[0-9]{4,}|Run /init|panic" "$LOG" | tail -30
