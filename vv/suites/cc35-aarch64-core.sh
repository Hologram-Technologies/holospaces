#!/usr/bin/env bash
#
# CC-35 — the system emulator executes the AArch64 (A64) integer ISA (ADR-021)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). The emulator's AArch64 core (holospaces::emulator::aarch64) is the
# implementation under test; the authorities are:
#   • the Arm Architecture Reference Manual (ARM DDI 0487) for the A64 base
#     instruction set + PSTATE.NZCV — exercised by real, toolchain-assembled A64
#     batteries that self-check every result (vv/artifacts/cc35/*.s → *.bin);
#   • qemu-aarch64 (linux-user) as the differential oracle: the *same* machine
#     code, run as a static _start ELF, must produce the same stdout + status.
# Witness: crates/holospaces/tests/cc35_aarch64.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CC35="$ROOT/vv/artifacts/cc35"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc35-aarch64-core: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# The committed binaries must match their sources (the assembler is external,
# not hand-encoded) — re-derive the checksums.
( cd "$CC35" && sha256sum -c cc35.sha256 >/dev/null ) \
    || { echo "cc35-aarch64-core: committed *.bin drift from cc35.sha256" >&2; exit 1; }

# The core witness: the AArch64 integer unit tests (the Arm-ARM-defined results
# for every instruction group) and the integration test that runs each real
# assembled battery to "PASS\n" + status 0.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --lib emulator::aarch64 -- --nocapture || exit 1
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc35_aarch64 -- --nocapture || exit 1

# The differential oracle: build the static ELFs and run each battery under
# qemu-aarch64, comparing stdout + status to the holospaces core's verdict
# ("PASS\n", 0). Re-derived live when the toolchain + qemu are present, so the
# oracle is never stale; pinned by the committed expected verdict otherwise.
if command -v qemu-aarch64 >/dev/null 2>&1; then
    if WITH_ELF=1 "$CC35/build.sh" >/dev/null 2>&1 && [ -e "$CC35/arith.elf" ]; then
        for b in arith memory control simd; do
            out="$(qemu-aarch64 "$CC35/$b.elf" 2>/dev/null)"; status=$?
            if [ "$out" != "PASS" ] || [ "$status" -ne 0 ]; then
                echo "cc35-aarch64-core: qemu-aarch64 differential FAILED for $b (out='$out' status=$status)" >&2
                exit 1
            fi
        done
        echo "cc35-aarch64-core: qemu-aarch64 differential PASS (oracle current)"
        rm -f "$CC35"/*.elf
    else
        echo "cc35-aarch64-core: qemu-aarch64 present but no aarch64 linker — differential skipped"
    fi
else
    echo "cc35-aarch64-core: qemu-aarch64 absent — differential pinned by the core witness (Arm-ARM results, per cc35/SOURCE.txt)"
fi
