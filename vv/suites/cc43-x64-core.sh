#!/usr/bin/env bash
#
# CC-43 — the x86-64 (AMD64) core: the system emulator's third ISA target
#
# Component conformance suite (arc42 ch.10). x86-64 is the ubiquitous registry
# architecture (most images publish linux/amd64), so this core lets the browser
# peer boot the largest share of real devcontainers. Like the RISC-V (CC-9) and
# AArch64 (CC-36) cores it is a CPU over the shared device bus (the κ-disk, the
# console) — no per-ISA device re-implementation (Law L4).
#
# This witness exercises the long-mode integer core at the instruction level (the
# analogue of the RISC-V core's riscv-tests): real x86-64 machine code decoded +
# executed — REX/ModRM/SIB-addressed arithmetic, a conditional branch loop that
# sets the zero flag, serial-console output (the 16550 at 0x3f8), and the stack
# (push/pop). Witness: crates/holospaces/src/emulator/x64.rs (mod tests).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc43-x64-core: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --lib x64::tests -- --nocapture \
    || exit 1

echo "cc43-x64-core: PASS"
