#!/usr/bin/env bash
#
# CC-36 — a real arm64 Linux kernel boots to userspace on the AArch64 emulator
# (ADR-021)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). The privileged AArch64 system (holospaces::emulator::aarch64 — the
# EL0/EL1 exception model, VMSAv8-64 paging, the ARM `virt` platform: GICv2, the
# generic timer, a PL011 console, PSCI) is the implementation under test. The
# authorities are:
#   • a real, unmodified arm64 Linux 6.6 kernel that must boot to userspace on
#     the emulator (the most stringent A64 + privileged correctness test;
#     vv/artifacts/cc36/linux/);
#   • qemu-system-aarch64 -M virt as the differential oracle — the same kernel
#     image, reaching the same userspace output byte-for-byte
#     (vv/artifacts/cc36/linux/expected-userspace.txt).
# Witness: crates/holospaces/tests/cc36_aarch64.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LINUX="$ROOT/vv/artifacts/cc36/linux"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc36-aarch64-linux: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# The committed kernel + oracle must match their checksums (reproducible build).
( cd "$LINUX" && sha256sum -c linux.sha256 >/dev/null ) \
    || { echo "cc36-aarch64-linux: artifact drift from linux.sha256" >&2; exit 1; }

# The OS-boot witness: a real arm64 Linux boots to userspace on the emulator,
# PID 1 prints its marker + /proc/version, and the machine powers off via PSCI.
# A full boot is ~1 min and must run optimised, so it is #[ignore]d in the cargo
# tier and run here in release.
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc36_aarch64 the_emulator_boots_real_arm64_linux_to_userspace \
    -- --ignored --nocapture || exit 1

# The differential oracle: qemu-system-aarch64 -M virt booting the same image
# must reach the same userspace output. expected-userspace.txt was captured from
# qemu; this step re-derives that capture live when qemu is present, so the
# oracle is never stale.
if command -v qemu-system-aarch64 >/dev/null 2>&1; then
    tmp="$(mktemp -d)"
    gzip -dc "$LINUX/Image.gz" > "$tmp/Image"
    timeout 90 qemu-system-aarch64 -M virt -cpu cortex-a57 -m 512M -nographic \
        -kernel "$tmp/Image" -append "console=ttyAMA0" 2>&1 \
        | tr -d '\r' > "$tmp/qemu.log"
    # Each expected userspace line (the marker + the real /proc/version) must
    # appear in qemu's console — checked independently rather than by adjacency,
    # since the kernel's own console messages can interleave between PID 1's
    # writes (a timing-dependent ordering that is not part of the oracle).
    ok=1
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        grep -aqF "$line" "$tmp/qemu.log" || ok=0
    done < "$LINUX/expected-userspace.txt"
    if [ "$ok" = 1 ]; then
        echo "cc36-aarch64-linux: qemu-system-aarch64 differential PASS (oracle current)"
    else
        echo "cc36-aarch64-linux: qemu-system-aarch64 differential FAILED — oracle drift" >&2
        echo "── qemu console (tail) ──" >&2; tail -40 "$tmp/qemu.log" >&2
        rm -rf "$tmp"; exit 1
    fi
    rm -rf "$tmp"
else
    echo "cc36-aarch64-linux: qemu-system-aarch64 absent — differential pinned in expected-userspace.txt (captured from qemu, per linux/SOURCE.txt)"
fi
