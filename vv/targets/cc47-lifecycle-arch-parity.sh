#!/usr/bin/env bash
#
# CC-47 (TARGET) — The lifecycle (suspend → κ snapshot → resume) works on every core
#
# OPM process: SD2 Lifecycle ("Suspending yields Snapshot; Resuming requires
# Snapshot → Holospace"). Today CC-30/CC-31 (suspend/resume from a κ snapshot)
# are witnessed on the RISC-V machine only. This target brings the AArch64 and
# x86-64 cores to lifecycle parity: a running guest suspends to a content-
# addressed snapshot (CPU + RAM + κ-disk) and resumes byte-identically.
#
# Authority: the CC-30 snapshot model (the substrate's content-addressed store as
#   the snapshot medium) applied to the AArch64 / x86-64 cores.
# Witness: crates/holospaces/tests/cc47_lifecycle_parity.rs — boot, run, suspend,
#   drop, resume; assert the resumed guest continues deterministically.
#
# GREEN when: suspend/resume round-trips on the AArch64 (and x86-64) cores — the
#   resumed CPU/RAM/disk state is byte-identical and execution continues.
#
# Status: TARGET — not yet live. Expected RED (non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
AARCH="$ROOT/crates/holospaces/src/emulator/aarch64.rs"
WITNESS="$ROOT/crates/holospaces/tests/cc47_lifecycle_parity.rs"

# Liveness probe: the aarch64 core has a snapshot/suspend surface AND the witness exists.
if grep -qE 'fn (suspend|snapshot|save_state)' "$AARCH" 2>/dev/null \
   && [ -f "$WITNESS" ]; then
    command -v cargo >/dev/null 2>&1 || { echo "cc47-lifecycle-arch-parity: SKIP — cargo absent"; exit 127; }
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc47_lifecycle_parity -- --ignored --nocapture || exit 1
    exit 0
fi

echo "cc47-lifecycle-arch-parity: RED — TARGET not yet live."
echo "  needed: suspend/resume (CPU+RAM+κ-disk snapshot) on the AArch64 + x86-64 cores;"
echo "          witness cc47_lifecycle_parity.rs."
echo "  spec:   a running guest suspends to a κ snapshot and resumes byte-identically,"
echo "          execution continuing (CC-30/CC-31 parity)."
exit 1
