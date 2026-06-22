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
#   qemu-system-x86_64 as the differential oracle. The boot exercises the SDM
#   Vol 3A system behaviors end-to-end: 4-level long-mode paging (+PCID), the IDT
#   / 8259 PIC / 8254 PIT / local APIC, the 16550 UART, interrupt + HLT-idle
#   delivery, and RDTSC/IA32_TSC.
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
CC44="$ROOT/vv/artifacts/cc44/linux"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc44-x64-linux: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# (a) The pinned fixtures must match their committed checksums — artifact drift
# (a re-built kernel that no longer matches the witnessed boot) fails here, not
# silently downstream.
( cd "$CC44" && sha256sum -c linux.sha256 >/dev/null ) \
    || { echo "cc44-x64-linux: artifact drift from linux.sha256" >&2; exit 1; }

# (b) The boot witness: the x86-64 core boots the committed vmlinux to userspace,
# byte-faithful to the committed qemu oracle (~release, #[ignore]d).
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc44_x64_boot an_amd64_linux_kernel_boots_to_userspace \
    -- --ignored --nocapture || exit 1

# (c) The differential oracle, re-derived live so it can never go stale: boot the
# SAME kernel under qemu-system-x86_64 and require it to emit exactly the lines
# the emulator was asserted against (expected-userspace.txt). The freestanding
# initramfs PID-1 is embedded in the kernel, so no -initrd is needed; PID 1
# powers off (LINUX_REBOOT_CMD_POWER_OFF → hlt) and qemu exits.
if command -v qemu-system-x86_64 >/dev/null 2>&1; then
    tmp="$(mktemp)"
    timeout 120 qemu-system-x86_64 -nographic -no-reboot -m 1G \
        -kernel "$CC44/bzImage" \
        -append "console=ttyS0 random.trust_cpu=on" 2>&1 | tr -d '\r' > "$tmp"
    miss=0
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        grep -qaF "$line" "$tmp" || { echo "cc44-x64-linux: qemu oracle missing: $line" >&2; miss=1; }
    done < "$CC44/expected-userspace.txt"
    if [ "$miss" -ne 0 ]; then
        echo "cc44-x64-linux: qemu-system-x86_64 differential FAILED (oracle drift)" >&2
        echo "── qemu console (tail) ──" >&2; tail -30 "$tmp" >&2
        rm -f "$tmp"; exit 1
    fi
    rm -f "$tmp"
    echo "cc44-x64-linux: qemu-system-x86_64 differential PASS (real amd64 Linux → userspace)"
else
    echo "cc44-x64-linux: qemu-system-x86_64 absent — differential pinned by expected-userspace.txt (per cc44/linux/SOURCE.txt)"
fi

echo "cc44-x64-linux: PASS"
