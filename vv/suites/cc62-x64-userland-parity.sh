#!/usr/bin/env bash
#
# CC-62 — the x86-64 core runs REAL userland workloads byte-correctly
#
# Component conformance suite (arc42 ch.10). The "seamlessly run any docker image
# as a .holo" promise rests on the x86-64 core being CORRECT, not just bootable.
# This suite is the standing regression gate for that: a battery of deterministic
# shell commands fed to the warm Alpine .holo shell must produce byte-exact,
# POSIX-correct output with ZERO kernel panics. It is behavioral — the repo's
# established x86-64 authority (CC-44 uses qemu as the differential) — so a silent
# miscompute in ANY layer (decode/execute, flags, SSE, the syscall ABI, or the
# kernel tty/pipe/fork paths) diverges the output and fails the gate. Kernel-mode
# code is covered for free: the pipes/forks/sort/awk/sed here drive real kernel
# paths, not just userspace.
#
# Authority: POSIX / coreutils-busybox defined behavior (independent of our
#   emulator), cross-checkable against qemu-x86_64 -L <cc45-rootfs> busybox sh -c
#   '<cmd>' (the CC-44 authority; see scratchpad/goldens.sh). 14 commands assert
#   byte-exact stdout; sha256sum asserts the known sha256("hello") digest.
#
# Witness: crates/holospaces/tests/cc62_x64_userland_parity.rs ::
#   x64_userland_workloads_are_byte_correct.
#
# Provenance note (why this suite exists): a multi-session "x86-64 printf escape
# crash" turned out to be a TEST-HARNESS ARTIFACT — a Rust b"..\\n.." literal
# collapsed to a literal newline, feeding a malformed command. The core was always
# correct (~17,900 instruction-effects self-checked vs the SDM, 0 divergence). This
# suite locks that in and enforces the discipline (raw-string commands; assert the
# bytes the guest actually receives).
#
# Depends on: CC-44/45 (amd64 boot), CC-59 (the warm-shell .holo fixture).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc62-x64-userland-parity: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc62_x64_userland_parity -- --nocapture || exit 1

echo "cc62-x64-userland-parity: PASS"
