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
# virtio-9p + virtio-net MMIO slots (raising the GIC) and exposes the bridge —
# one devbus, two MMIO transports, no per-ISA device re-implementation.
#
# Authority: the 9P2000.L / OASIS VirtIO v1.2 specs + the CC-15/CC-16/CC-33
#   authorities, run against the AArch64 core's own virtio-mmio transport.
# Witness: crates/holospaces/tests/cc46_devbus_parity.rs — the aarch64 core
#   mounts a 9p export and shares files, NATs an outbound TCP flow, and exposes
#   the in-process guest bridge, all through the shared devbus.
#
# Regression guard: the RISC-V CC-15/CC-16/CC-33 suites stay green through the
#   behavior-preserving extraction (verified by those suites; not re-run here).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc46-devbus-arch-parity: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc46_devbus_parity -- --nocapture || exit 1
