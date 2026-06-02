#!/usr/bin/env bash
#
# Perf witness — emulator boot throughput (P2 stages A–D).
#
# Records how fast the interpreter boots a real RISC-V Linux to userspace, so the
# throughput the optimization stages bought (RAM fast path, bulk memory, the
# software TLB, elided interrupt-latch writes) is a recorded number in the CI log
# rather than a claim. NOT a tight gate — CI-runner speed varies several-fold —
# so the probe asserts only a catastrophe floor (a >10x regression, e.g. the TLB
# silently disabled); the printed MIPS is the real signal for tracking.
#
# Authority: the same pinned Linux kernel + qemu-differential-validated emulator
# the CC-9 witness uses; the measured workload is byte-identical run to run.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "perf-throughput: SKIP — cargo unavailable" >&2; exit 127; fi
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test p2_throughput emulator_boots_real_linux_throughput \
    -- --ignored --nocapture || exit 1
