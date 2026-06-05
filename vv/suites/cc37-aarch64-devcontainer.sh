#!/usr/bin/env bash
#
# CC-37 — an arm64 devcontainer runs the ecosystem's stock linux-arm64 binaries
# (ADR-021)
#
# Component conformance suite (arc42 chapter 10). The AArch64 system
# (holospaces::emulator::aarch64) boots an arm64 devcontainer from a κ-disk
# virtio-blk rootfs — the **same** substrate-backed virtio device the RISC-V
# machine boots (the shared emulator::devbus, no per-ISA re-implementation) — and
# runs a **stock, unmodified linux-arm64 busybox** binary. Authorities:
#   • the Dev Container + OCI image specs over an arm64/linux rootfs assembled
#     into the κ-disk (CC-7) by the in-crate Layer Assembler;
#   • a stock linux-arm64 busybox (vv/artifacts/cc37/rootfs/) — arbitrary
#     linux-arm64 binaries run with no riscv64 workaround;
#   • qemu-system-aarch64 -M virt as the differential oracle.
# Witness: crates/holospaces/tests/cc37_aarch64.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CC37="$ROOT/vv/artifacts/cc37"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc37-aarch64-devcontainer: SKIP — cargo not available in this environment" >&2
    exit 127
fi

( cd "$CC37" && sha256sum -c cc37.sha256 >/dev/null ) \
    || { echo "cc37-aarch64-devcontainer: artifact drift from cc37.sha256" >&2; exit 1; }

# The devcontainer-boot witness: a real arm64 Linux mounts the κ-disk virtio-blk
# rootfs and runs the stock linux-arm64 busybox as PID 1 (~release, #[ignore]d).
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc37_aarch64 an_arm64_devcontainer_runs_a_stock_linux_arm64_binary \
    -- --ignored --nocapture || exit 1

# The same boot, but the κ-disk is PAGED from a KappaStore by streaming sectors —
# the exact path the browser peer's Aarch64Workspace takes from OPFS (no full image
# in RAM). Proves the streamed paged κ-disk boots the AArch64 core (~release).
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc37_aarch64 an_arm64_devcontainer_boots_paged_from_a_kappa_store \
    -- --ignored --nocapture || exit 1

# The differential oracle: qemu-system-aarch64 -M virt booting the same kernel +
# rootfs must run the same stock binary to the same markers.
if command -v qemu-system-aarch64 >/dev/null 2>&1; then
    tmp="$(mktemp -d)"
    gzip -dc "$CC37/linux/Image.gz" > "$tmp/Image"
    # Reproduce the rootfs the witness assembles, as a raw disk for qemu.
    if cargo run --release --quiet --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --example cc37_mkrootfs -- "$tmp/rootfs.ext4" 2>/dev/null; then
        timeout 120 qemu-system-aarch64 -M virt -cpu cortex-a57 -m 512M -nographic \
            -kernel "$tmp/Image" -append "console=ttyAMA0 root=/dev/vda rw init=/init" \
            -drive file="$tmp/rootfs.ext4",format=raw,if=none,id=hd0 \
            -device virtio-blk-device,drive=hd0 2>&1 | tr -d '\r' > "$tmp/qemu.log"
        if grep -aq 'CC37-ARCH:aarch64' "$tmp/qemu.log" \
           && grep -aq 'CC37-COMPUTE:500500' "$tmp/qemu.log"; then
            echo "cc37-aarch64-devcontainer: qemu-system-aarch64 differential PASS (stock arm64 binary ran)"
        else
            echo "cc37-aarch64-devcontainer: qemu-system-aarch64 differential FAILED" >&2
            echo "── qemu console (tail) ──" >&2; tail -40 "$tmp/qemu.log" >&2
            rm -rf "$tmp"; exit 1
        fi
    else
        echo "cc37-aarch64-devcontainer: (rootfs export helper unavailable — qemu differential skipped)"
    fi
    rm -rf "$tmp"
else
    echo "cc37-aarch64-devcontainer: qemu-system-aarch64 absent — differential pinned by the in-emulator witness (per cc37/SOURCE.txt)"
fi
