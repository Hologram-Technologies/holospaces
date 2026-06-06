#!/usr/bin/env bash
#
# CC-46 — the shared device bus serves 9p, network, and the guest bridge to
#         EVERY core (arch device parity)
#
# Component conformance suite (arc42 ch.10). OPM process: SD3 Sync (network) +
# SD4 Working (the 9p workspace + the bridge a projection drives). Law L4: the
# substrate's devices are shared, not per-ISA. The virtio-9p (CC-15), virtio-net
# + userspace NAT (CC-16), and the in-process guest bridge (CC-33) servicing
# lives in the core-agnostic emulator::devbus; the AArch64 core wires the
# virtio-9p + virtio-net MMIO slots (raising the GIC) and the x86-64 core wires
# the same slots over its own MMIO window — one devbus, three MMIO transports
# (RISC-V PLIC, AArch64 GIC, x86-64), no per-ISA device re-implementation.
#
# GREEN at the REAL-KERNEL caliber of CC-15/CC-16/CC-33 (real-OS boots):
#   • cc46_realboot — a real arm64 Linux boots over the shared devbus and a real
#     guest userspace mounts the 9p workspace (file round-trip), opens an
#     outbound TCP/HTTP flow over virtio-net through the NAT, and serves a real
#     listener reached from the host over the in-process bridge (round-trip).
# The device-level fast checks (cc46_devbus_parity, both cores) are regression
# coverage that each core's MMIO transport reaches the shared devbus — necessary
# but NOT the parity witness (no kernel boot).
#
# Authority: the 9P2000.L / OASIS VirtIO v1.2 specs + the CC-15/CC-16/CC-33
#   authorities; the differential oracle is qemu-system-aarch64 -M virt on the
#   same kernel + rootfs + the 10.0.2.0/24 user-mode NAT model.
# Fixtures: vv/artifacts/cc46 (build.sh + cc46.sha256, recorded in PROVENANCE).
#
# x86-64 real-boot parity follows once #12 (CC-43/44 long-mode boot) lands; the
# device-level x64 check stays as regression coverage.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CC46="$ROOT/vv/artifacts/cc46"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc46-devbus-arch-parity: SKIP — cargo not available in this environment" >&2
    exit 127
fi

( cd "$CC46" && sha256sum -c cc46.sha256 >/dev/null ) \
    || { echo "cc46-devbus-arch-parity: artifact drift from cc46.sha256" >&2; exit 1; }

# The device-level regression coverage: each core's virtio-mmio transport reaches
# the shared devbus (9p mount, NAT outbound, bridge) — fast, default test set.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc46_devbus_parity -- --nocapture || exit 1

# The CC-46 PARITY witness (CC-15/CC-16/CC-33 caliber): a REAL arm64 Linux boots
# over the shared devbus and a real guest userspace exercises 9p + net + bridge
# (~release, #[ignore]d).
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc46_realboot the_aarch64_core_serves_9p_net_and_bridge_to_a_real_arm64_boot \
    -- --ignored --nocapture || exit 1
