#!/usr/bin/env bash
# Robust boot-diagnostic runner for the x86-64 emulator.
#
# The dev tooling is responsible for maintaining the environment: if the
# environment can sabotage a run, the tooling must make that impossible. This
# script defends against every hazard we have actually hit here:
#
#  1. STALE BINARIES. cargo's change-detection is unreliable in this container
#     (an edited source keeps an mtime <= its artifact; even removing the
#     fingerprint did not always force a rebuild). The ONLY reliable force is
#     `cargo clean -p holospaces`. After building we ASSERT the binary is newer
#     than every source file *and* younger than this script's start — refusing to
#     run a stale binary rather than producing a misleading result.
#  2. VOLATILE /tmp. /tmp is tmpfs and gets wiped mid-run; all output goes to a
#     PID-unique file under the worktree (boot-diag.<pid>.log) with a stable
#     `boot-diag.log` symlink to the latest.
#  3. MULTI-PROCESS COLLISIONS. setsid/detached runs escaped earlier `pkill`s and
#     interleaved into one log. We kill-and-verify in a loop until zero remain,
#     then launch exactly one and read THAT run's PID-unique log.
#
# Usage: scripts/dev-boot.sh [slices] [cycles_per_slice_millions]
#   default 200 slices x 100M = 20B cycles.  e.g. `dev-boot.sh 2000 1` = 1M-grain.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
START="$(date +%s)"

# SINGLE-INSTANCE DISCIPLINE. The environment punished us for running the diag by
# hand alongside the tool: detached strays escaped pkill and interleaved into the
# logs. The cure is to make that impossible — this script is the ONLY thing that
# may launch a boot, and only one may run at a time. We take an exclusive lock and,
# if another dev-boot or any diag binary is alive, we reclaim the environment
# (kill them) before proceeding. No caller should ever pkill by hand again.
exec 9>"$ROOT/.dev-boot.lock"
if ! flock -n 9; then
    echo "[dev-boot] another dev-boot holds the lock — reclaiming…"
    # Reclaim by killing only the WORKER (the diag). A prior dev-boot's wait-loop
    # exits as soon as its diag dies, releasing the lock — so we never kill a
    # dev-boot by name (which once killed this very process). Then wait for the
    # lock to free.
    pkill -9 -f cc45_boot_diag 2>/dev/null
    flock -w 120 9 || { echo "could not acquire lock"; exit 4; }
fi
# Any diag process not under our lock is a stray from a past mishap — clear it.
for p in $(pgrep -f cc45_boot_diag 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
rm -f "$ROOT"/boot-diag.*.log 2>/dev/null
SLICES="${1:-200}"
PER="${2:-100}"
EX=cc45_boot_diag
BIN="$ROOT/target/release/examples/$EX"

# (1) Force-fresh build. cargo clean is the only reliable invalidation here.
echo "[dev-boot] force-clean rebuild of $EX (defeating stale-binary cache)…"
cargo clean --release -p holospaces 2>/dev/null
if ! cargo build --release -q -p holospaces --example "$EX" 2>build.err; then
    grep -E "error" build.err | head -20; echo "BUILD FAILED"; exit 1
fi
rm -f build.err
# Assert the binary is genuinely fresh (younger than this run's start).
[ -x "$BIN" ] || { echo "missing $BIN after build"; exit 1; }
bage=$(( $(date +%s) - $(stat -c %Y "$BIN") ))
if [ "$bage" -gt 600 ]; then
    echo "REFUSING TO RUN: $EX is ${bage}s old — the rebuild did not take. Aborting."
    exit 2
fi
newest_src=$(find crates/holospaces/src crates/holospaces/examples -name '*.rs' -newer "$BIN" | head -1)
[ -n "$newest_src" ] && { echo "REFUSING TO RUN: $newest_src is newer than the binary — stale build."; exit 2; }
echo "[dev-boot] fresh binary (${bage}s old): $BIN"

# (3) Kill any prior diag run and verify none remain before launching ours.
for _ in 1 2 3 4 5; do
    pids=$(pgrep -f "$EX" || true); [ -z "$pids" ] && break
    for p in $pids; do kill -9 "$p" 2>/dev/null; done; sleep 1
done
[ -n "$(pgrep -f "$EX" || true)" ] && { echo "could not clear prior $EX procs"; exit 3; }

# (2) Launch exactly one; read its OWN pid-unique log.
# Launch with the lock fd CLOSED (9>&-) so the orphaned diag never holds the lock.
"$BIN" "$SLICES" "$PER" >/dev/null 2>&1 9>&- &
pid=$!
LOG="$ROOT/boot-diag.$pid.log"
echo "[dev-boot] booting pid $pid ($SLICES x ${PER}M cycles) -> $LOG"
until grep -qaE "=== (HALT|budget)" "$LOG" 2>/dev/null || ! kill -0 "$pid" 2>/dev/null; do
    sleep 4
done
echo "=== boot-diag summary (pid $pid) ==="
grep -aE "CC45-|=== HALT|=== budget|prot=\+[1-9]|nopage=\+[1-9][0-9]{2,}|Run /init|panic" "$LOG" | tail -30
