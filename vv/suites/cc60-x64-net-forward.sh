#!/usr/bin/env bash
#
# CC-60 — the x86-64 core's port-forward + loopback reachability control plane is
#         at parity with the riscv/aarch64 cores (CC-16 + CC-21 + CC-33)
#
# Component conformance suite (arc42 ch.10). A docker SERVER image is only useful
# if its listening socket is reachable. The riscv core reaches a guest server over
# the shared `net` NAT two ways: an external host-socket forward
# (Emulator::attach_net_forward + net::StdIngress, CC-21) and the in-process
# loopback bridge (enable_loopback / dial_guest / guest_send / guest_recv, CC-33).
# The x86-64 core already had the loopback bridge; this witnesses the newly-added
# Cpu::attach_net_forward (the external-forward parity) and that the whole
# reachability control plane functions on the x64 core — without a distro boot.
#
# Witness: crates/holospaces/tests/cc60_x64_net_forward.rs ::
#   x64_port_forward_and_loopback_control_plane_is_at_parity (+ the attach_net /
#   attach_net_forward device-install parity). Fast (no boot).
#
# Scope: this gates the CONTROL PLANE. The full behavioural proof — a real amd64
# server brought up over virtio-net returning bytes to a host client — is the heavy
# target vv/targets/cc60-x64-net-reachable-server.sh (RED until an amd64 server
# fixture exists; the x64 virtio-net path has not yet been driven by a real guest).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc60-x64-net-forward: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --test cc60_x64_net_forward -- --nocapture || exit 1

echo "cc60-x64-net-forward: PASS"
