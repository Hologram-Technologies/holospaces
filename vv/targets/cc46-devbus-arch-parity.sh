#!/usr/bin/env bash
#
# CC-46 (TARGET) — The shared device bus serves 9p, network, and the guest bridge
#                  to EVERY core (arch device parity)
#
# OPM process: SD3 Sync (network) + SD4 Working (the 9p workspace + the bridge a
# projection drives). Law L4: the substrate's devices are shared, not per-ISA.
# Today only the RISC-V machine wires virtio-9p (CC-15), virtio-net + NAT
# (CC-16), and the in-process guest bridge (CC-33); the AArch64 core wires only
# virtio-blk, and the x86-64 core none. This target moves 9p/net/bridge into the
# shared emulator::devbus and wires the AArch64 and x86-64 cores to it.
#
# NOTE (docs-vs-code): CC-37/ADR-021 prose claims the aarch64 core has 9p+net+
#   bridge, but emulator/aarch64.rs::Sys has only uart+virtio-blk. This target is
#   the V&V that makes that claim TRUE (or the catalog is corrected — the V&V is
#   the arbiter).
#
# Authority: the virtio-9p / virtio-net specs and the CC-15 / CC-16 / CC-33
#   authorities, run against the AArch64 (and x86-64) cores.
# Witness: crates/holospaces/tests/cc46_devbus_parity.rs — the aarch64 core
#   mounts a 9p export, NATs an outbound TCP flow, and answers a bridge dial.
#
# GREEN when: emulator::devbus exposes 9p + net + bridge, and the aarch64 (and
#   x86-64) cores pass CC-15/CC-16/CC-33-equivalent witnesses with no per-ISA
#   device reimplementation.
#
# Status: TARGET — not yet live. Expected RED (non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DEVBUS="$ROOT/crates/holospaces/src/emulator/devbus.rs"
AARCH="$ROOT/crates/holospaces/src/emulator/aarch64.rs"
WITNESS="$ROOT/crates/holospaces/tests/cc46_devbus_parity.rs"

# Liveness probe: devbus exposes net + 9p servicing AND the aarch64 Sys wires them.
if grep -qE 'net_mmio|9p_mmio|p9_mmio' "$DEVBUS" 2>/dev/null \
   && grep -qE 'VIRTIO_NET_BASE|VIRTIO_9P_BASE|VIRTIO_P9_BASE' "$AARCH" 2>/dev/null \
   && [ -f "$WITNESS" ]; then
    command -v cargo >/dev/null 2>&1 || { echo "cc46-devbus-arch-parity: SKIP — cargo absent"; exit 127; }
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc46_devbus_parity -- --ignored --nocapture || exit 1
    exit 0
fi

echo "cc46-devbus-arch-parity: RED — TARGET not yet live."
echo "  observed: emulator/aarch64.rs::Sys wires only virtio-blk (+uart); devbus has only blk."
echo "  needed:   extract 9p + net + bridge into the shared emulator::devbus and wire the"
echo "            AArch64 + x86-64 cores; witness cc46_devbus_parity.rs."
echo "  spec:     the aarch64 (and x86-64) core mounts 9p, NATs an outbound TCP flow, and"
echo "            answers a bridge dial — CC-15/CC-16/CC-33 parity, no per-ISA device code."
exit 1
