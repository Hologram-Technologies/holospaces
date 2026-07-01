#!/usr/bin/env bash
#
# CC-60 (TARGET) — a real amd64 docker SERVER image is REACHABLE from the host
#
# OPM process: SD5 Devcontainer Provisioning + the running-app preview. The
# behavioural completion of CC-60's control-plane parity: a real amd64 server
# image boots on the x86-64 core with virtio-net up, listens on a port, and a host
# client reads its real response over the in-process loopback bridge (CC-33) or an
# external host-socket forward (CC-21). Composes with CC-59 warm-resume for speed
# (resume a "server listening" snapshot instead of re-booting).
#
# Authority: the server's own response bytes (and qemu-system-x86_64 running the
#   identical disk as the differential, as CC-44). NOT a self-read of the NAT.
# Witness (to write): crates/holospaces/tests/cc60_x64_reachable_server.rs ::
#   a_real_amd64_server_image_is_reachable_over_the_loopback_bridge.
# Depends on: CC-44/45 (amd64 boot), CC-60 control plane (Cpu::attach_net_forward,
#   the loopback bridge), CC-59 (warm resume, optional speed path).
#
# RECIPE (derived; the next session's precise starting point):
#   * NIC discovery: append `virtio_mmio.device=0x200@0xd0000400:12` to the kernel
#     cmdline (VIRTIO_NET_BASE=0xD000_0400, VIRTIO_NET_IRQ=12 in emulator/x64.rs),
#     alongside the existing virtio-blk `…@0xd0000000:11`.
#   * Guest network: the NAT is slirp 10.0.2.0/24 (GUEST_IP 10.0.2.15, gateway
#     10.0.2.2, DNS 10.0.2.3 — emulator/net.rs). The guest `/init` brings eth0 up
#     via DHCP (busybox udhcpc) or statically (`ip addr add 10.0.2.15/24 dev eth0`).
#   * Server: the Alpine minirootfs (vv/artifacts/cc45/alpine) ships busybox; an
#     `/init` runs `busybox httpd -p 80 -h /www` serving a known body
#     (e.g. HELLO-FROM-AMD64-SERVER), then blocks.
#   * Host side: boot with attach_net (NoEgress) + enable_loopback, then
#     dial_guest(80) / guest_send("GET / …") / pump / guest_recv → assert the body.
#
# RISK: the x64 virtio-net device emulation has NOT been driven by a real guest
#   driver end-to-end (only the in-process loopback control plane is exercised, CC-60
#   suite). Expect to debug the virtio-net RX/TX virtqueue + feature negotiation
#   against the real Linux virtio_net driver. Heavy: boots a distro (~tens of
#   seconds release); belongs in vv/heavy/ once green.
#
# Status: TARGET — RED until the amd64 server fixture + witness exist. Non-gating.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WITNESS="$ROOT/crates/holospaces/tests/cc60_x64_reachable_server.rs"

if [ -f "$WITNESS" ] && grep -q 'a_real_amd64_server_image_is_reachable_over_the_loopback_bridge' "$WITNESS" 2>/dev/null; then
    command -v cargo >/dev/null 2>&1 || { echo "cc60-x64-net-reachable-server: SKIP — cargo absent"; exit 127; }
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
        --test cc60_x64_reachable_server -- --ignored --nocapture || exit 1
    exit 0
fi

echo "cc60-x64-net-reachable-server: RED — TARGET not yet live."
echo "  needed: an amd64 server fixture (busybox httpd /init + virtio-net bring-up)"
echo "          and crates/holospaces/tests/cc60_x64_reachable_server.rs."
echo "  spec:   a real amd64 server image boots on the x64 core and a host client"
echo "          reads its real response over the loopback bridge / forward."
echo "  recipe: virtio_mmio …@0xd0000400:12 · NAT 10.0.2.15/gw 10.0.2.2 · busybox httpd."
exit 1
