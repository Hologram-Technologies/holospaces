#!/usr/bin/env bash
#
# CC-44 — A real amd64 (x86-64) Linux kernel boots to userspace on the emulator
#
# OPM process: SD2 Booting ("Booting requires Holospace + Substrate" → a running
# OS). The conceptual model is arch-agnostic; the ISA is a property of the
# Holospace (ADR-021). This is the x86-64 realization of CC-36 (aarch64) / CC-9
# (riscv64) — the third ISA boots a real Linux to userspace.
#
# Authority: a stock, unmodified upstream x86-64 Linux 6.6 kernel
#   (vv/artifacts/cc44/linux/vmlinux.gz) + a freestanding initramfs PID-1, with
#   qemu-system-x86_64 as the differential oracle. The emulator's userspace
#   output (the marker + the real /proc/version) must match what qemu printed
#   booting the same kernel (vv/artifacts/cc44/linux/expected-userspace.txt,
#   re-derived live from qemu whenever it is present, so the oracle is never
#   stale). The boot exercises the SDM Vol 3A system behaviors end-to-end:
#   4-level long-mode paging (+PCID), the IDT / 8259 PIC / 8254 PIT / local APIC,
#   the 16550 UART, interrupt delivery, RDTSC/IA32_TSC, and the HLT idle path.
#
# Witness: crates/holospaces/tests/cc44_x64_boot.rs
#   `an_amd64_linux_kernel_boots_to_userspace` (#[ignore]d so the default unit
#   run stays fast; this suite runs it explicitly, release). The kernel reaches
#   `Run /init`, and PID 1 prints HOLOSPACES-LINUX-USERSPACE-OK + its real
#   /proc/version, byte-identical to qemu.
#
# Promoted from vv/targets/ once green (the x86-64 core gained the 64-bit Linux
# boot protocol, virtio-mmio κ-disk servicing over the shared emulator::devbus,
# and architecturally-correct interrupt + HLT-idle delivery — Law L4).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc44-x64-linux: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc44_x64_boot an_amd64_linux_kernel_boots_to_userspace \
    -- --ignored --nocapture || exit 1

echo "cc44-x64-linux: PASS"
