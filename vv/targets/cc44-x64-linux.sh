#!/usr/bin/env bash
#
# CC-44 (TARGET) — A real amd64 (x86-64) Linux kernel boots to userspace
#
# OPM process: SD2 Booting ("Booting requires Holospace + Substrate" → a running
# OS). The conceptual model is arch-agnostic; the ISA is a property of the
# Holospace (ADR-021). This is the x86-64 realization of CC-36 (aarch64) / CC-9
# (riscv64) — the third ISA boots a real Linux to userspace.
#
# Authority: a stock upstream x86-64 Linux kernel + a minimal userland, with
#   qemu-system-x86_64 as the differential oracle (the CC-36 pattern).
# Witness (written first when this item is built): crates/holospaces/tests/
#   cc44_x64_boot.rs — `an_amd64_linux_kernel_boots_to_userspace` (#[ignore]d,
#   release): the x86-64 core boots vv/artifacts/cc44 to a userspace marker.
# Fixture recipe: vv/artifacts/cc44/build.sh (mirrors cc36/linux).
#
# GREEN when: the x86-64 core gains the 64-bit Linux boot protocol + virtio-mmio
#   κ-disk servicing + interrupt delivery (IDT) over the shared emulator::devbus,
#   boots the amd64 fixture, and prints the userspace marker — byte-faithful to
#   qemu-system-x86_64.
#
# Status: TARGET — not yet live. Expected RED (the target tier is non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
X64="$ROOT/crates/holospaces/src/emulator/x64.rs"
WITNESS="$ROOT/crates/holospaces/tests/cc44_x64_boot.rs"
FIXTURE="$ROOT/vv/artifacts/cc44/linux/bzImage"

# Liveness probe (cheap): the boot entry + fixture + witness all exist.
if grep -q 'fn boot_linux' "$X64" 2>/dev/null \
   && [ -f "$FIXTURE" ] && [ -f "$WITNESS" ]; then
    command -v cargo >/dev/null 2>&1 || { echo "cc44-x64-linux: SKIP — cargo absent"; exit 127; }
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_linux_kernel_boots_to_userspace \
        -- --ignored --nocapture || exit 1
    exit 0
fi

echo "cc44-x64-linux: RED — TARGET not yet live."
echo "  needed: x86-64 64-bit boot protocol + virtio-mmio (devbus) + IDT in x64.rs;"
echo "          fixture vv/artifacts/cc44 (amd64 kernel+userland); witness cc44_x64_boot.rs."
echo "  spec:   the x86-64 core boots a real amd64 Linux to a userspace marker"
echo "          (qemu-system-x86_64 differential)."
exit 1
