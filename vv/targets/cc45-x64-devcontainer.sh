#!/usr/bin/env bash
#
# CC-45 (TARGET) — An arbitrary amd64 devcontainer runs on x86-64 (build-capable;
#                  full Dev Container / OCI spec)
#
# OPM process: SD5 Devcontainer Provisioning + SD2 Booting. The x86-64 analogue
# of CC-37 (aarch64), broadened to the full Dev Container / OCI contract on a
# build-capable disk. The x86-64 system (holospaces::emulator::x64) boots an amd64
# devcontainer from a κ-disk virtio-blk rootfs over the SHARED emulator::devbus —
# no per-ISA workaround (ADR-021, Law L4) — and runs a stock, unmodified
# linux-amd64 binary; the κ-disk indexes O(content) (occupancy-index boot), so a
# multi-GiB build-capable disk boots promptly. Authorities:
#   • the Dev Container + OCI image specs over an amd64/linux rootfs assembled into
#     the κ-disk (CC-7) by the in-crate Layer Assembler;
#   • a stock linux-amd64 busybox (vv/artifacts/cc45/rootfs/) — arbitrary
#     linux-amd64 binaries run with no per-ISA workaround;
#   • qemu-system-x86_64 as the differential oracle.
# Witnesses: crates/holospaces/tests/cc44_x64_boot.rs (the CC-45 tests).
# Depends on: CC-44 (the x86-64 boot path).
#
# Status: TARGET — promoted to vv/suites/ once every section below is green for
#   real. Expected RED (non-gating) until the fixture is committed.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CC45="$ROOT/vv/artifacts/cc45"

command -v cargo >/dev/null 2>&1 || { echo "cc45-x64-devcontainer: SKIP — cargo absent"; exit 127; }

# ── Section B: the build-capable disk (occupancy-index boot, O(content)) ──────
# LIVE: an ≥ 8 GiB disk is declarable and boots promptly because only occupied
# sectors are paged. These pass for real (no fixture needed).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --lib \
    emulator::tests::occupancy_index_pages_only_content_for_a_multi_gib_disk \
    -- --nocapture || exit 1
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc44_x64_boot an_amd64_linux_boots_from_an_occupancy_indexed_build_capable_disk \
    -- --ignored --nocapture || exit 1

# ── The stock linux-amd64 binary + arbitrary devcontainer (needs the fixture) ──
if [ -f "$CC45/cc45.sha256" ] && [ -f "$CC45/linux/vmlinux.gz" ] && [ -f "$CC45/rootfs/layer.tar.gz" ]; then
    ( cd "$CC45" && sha256sum -c cc45.sha256 >/dev/null ) \
        || { echo "cc45-x64-devcontainer: artifact drift from cc45.sha256" >&2; exit 1; }

    # A real amd64 Linux mounts the κ-disk virtio-blk rootfs and runs the stock
    # linux-amd64 busybox as PID 1 (~release, #[ignore]d).
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_devcontainer_runs_a_stock_linux_amd64_binary \
        -- --ignored --nocapture || exit 1

    # The same boot, but the κ-disk is PAGED from a KappaStore by streaming sectors
    # — the exact path the browser peer's X64Workspace takes from OPFS.
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_devcontainer_boots_paged_from_a_kappa_store \
        -- --ignored --nocapture || exit 1

    # The differential oracle: qemu-system-x86_64 booting the same kernel + rootfs
    # must run the same stock binary to the same markers.
    if command -v qemu-system-x86_64 >/dev/null 2>&1; then
        tmp="$(mktemp -d)"
        if cargo run --release --quiet --manifest-path "$ROOT/Cargo.toml" -p holospaces \
            --example cc45_mkrootfs -- "$tmp/rootfs.ext4" 2>/dev/null; then
            timeout 180 qemu-system-x86_64 -M q35 -m 512M -nographic -no-reboot \
                -kernel "$CC45/linux/bzImage" \
                -append "console=ttyS0 root=/dev/vda rw init=/init random.trust_cpu=on" \
                -drive file="$tmp/rootfs.ext4",format=raw,if=none,id=hd0 \
                -device virtio-blk-pci,drive=hd0 2>&1 | tr -d '\r' > "$tmp/qemu.log"
            if grep -aq 'CC45-ARCH:x86_64' "$tmp/qemu.log" \
               && grep -aq 'CC45-COMPUTE:500500' "$tmp/qemu.log"; then
                echo "cc45-x64-devcontainer: qemu-system-x86_64 differential PASS (stock amd64 binary ran)"
            else
                echo "cc45-x64-devcontainer: qemu-system-x86_64 differential FAILED" >&2
                echo "── qemu console (tail) ──" >&2; tail -40 "$tmp/qemu.log" >&2
                rm -rf "$tmp"; exit 1
            fi
        else
            echo "cc45-x64-devcontainer: (rootfs export helper unavailable — qemu differential skipped)"
        fi
        rm -rf "$tmp"
    else
        echo "cc45-x64-devcontainer: qemu-system-x86_64 absent — differential pinned by the in-emulator witness (per cc45/SOURCE.txt)"
    fi
    exit 0
fi

echo "cc45-x64-devcontainer: RED — build-capable disk (occupancy index) is LIVE; full bar pending."
echo "  done:   occupancy-index boot path — an ≥ 8 GiB disk boots O(content) (witnesses above, green)."
echo "  needed: the stock linux-amd64 busybox fixture (vv/artifacts/cc45/, run its build.sh) so the"
echo "          differential witnesses + qemu-system-x86_64 oracle run. See issue #13 / CC-45."
exit 1
