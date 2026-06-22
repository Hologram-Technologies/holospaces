#!/usr/bin/env bash
#
# Perf witness — x86-64 guest throughput (the CC-48 fast-path baseline).
#
# Records how fast the x86-64 interpreter boots a real amd64 Linux to userspace,
# so the throughput the substrate fast-execution path (the x86-64 → wasm DBT)
# must clear for CC-48 is a recorded number in the CI log rather than a claim.
# CC-48 runs the real openvscode-server (Node/V8) INSIDE the booted guest, which
# the plain interpreter cannot drive at interactive speed — this probe is the
# measured "before", and the fast path's win is the "after" ratio.
#
# NOT a tight gate — CI-runner speed varies several-fold — so the probe asserts
# only a catastrophe floor (a real boot retires >100M guest instructions); the
# printed guest MIPS is the real signal for tracking.
#
# Authority: the same pinned amd64 Linux kernel the CC-44 witness boots
# (qemu-differential-validated); the measured workload is byte-identical run to
# run (boot to PID-1 power-off).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "perf-x64-throughput: SKIP — cargo unavailable" >&2; exit 127; fi
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc48_x64_throughput x64_guest_throughput_booting_real_linux \
    -- --ignored --nocapture || exit 1
