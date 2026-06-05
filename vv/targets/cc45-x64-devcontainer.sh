#!/usr/bin/env bash
#
# CC-45 (TARGET) — An amd64 devcontainer runs the ecosystem's stock linux-amd64 binaries
#
# OPM process: SD5 Devcontainer Provisioning + SD2 Booting. The x86-64 analogue
# of CC-37 (aarch64): a real amd64 devcontainer boots from a κ-disk virtio-blk
# rootfs over the SHARED emulator::devbus and runs a stock, unmodified
# linux-amd64 binary — no per-ISA workaround (ADR-021, Law L4).
#
# Authority: the Dev Container + OCI image specs over an amd64/linux image; a
#   stock linux-amd64 busybox as the unmodified binary; qemu-system-x86_64 -M q35
#   (or -M pc) as the differential oracle.
# Witness: crates/holospaces/tests/cc44_x64_boot.rs ::
#   `an_amd64_devcontainer_runs_a_stock_linux_amd64_binary` (#[ignore]d, release).
# Depends on: CC-44 (the x86-64 boot path).
#
# GREEN when: an amd64 devcontainer boots on the x86-64 core and a stock
#   linux-amd64 busybox runs as PID 1 (`uname -m` → x86_64, a shell compute, head
#   reading /proc/version), byte-faithful to qemu-system-x86_64.
#
# Status: TARGET — not yet live. Expected RED (non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WITNESS="$ROOT/crates/holospaces/tests/cc44_x64_boot.rs"
ROOTFS="$ROOT/vv/artifacts/cc45/rootfs"

if [ -f "$WITNESS" ] && grep -q 'an_amd64_devcontainer_runs_a_stock_linux_amd64_binary' "$WITNESS" 2>/dev/null \
   && [ -d "$ROOTFS" ]; then
    command -v cargo >/dev/null 2>&1 || { echo "cc45-x64-devcontainer: SKIP — cargo absent"; exit 127; }
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_devcontainer_runs_a_stock_linux_amd64_binary \
        -- --ignored --nocapture || exit 1
    exit 0
fi

echo "cc45-x64-devcontainer: RED — TARGET not yet live."
echo "  needed: CC-44 boot path; a stock linux-amd64 busybox fixture (vv/artifacts/cc45/rootfs)."
echo "  spec:   an amd64 devcontainer boots on the x86-64 core and runs an unmodified"
echo "          linux-amd64 binary (qemu-system-x86_64 differential)."
exit 1
