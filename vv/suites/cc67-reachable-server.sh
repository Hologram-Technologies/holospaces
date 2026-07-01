#!/usr/bin/env bash
#
# CC-67 / CC-60 (behavioral) — a real server image is REACHABLE from the host on x86-64
#
# Component conformance suite (arc42 ch.10). The behavioral completion of CC-60's control-plane parity and
# the "reachable link" pillar of "run any docker image": the CC-65 pipeline boots a server image with a
# virtio-net NIC, the guest's real app binds 0.0.0.0:8080, and a real client OUTSIDE the guest reaches it
# through the κ-native NAT ingress. Two proofs, both over the same guest virtio-net RX + poll_ingress path:
#   • host socket  — a real std::net::TcpStream to a StdIngress-forwarded 127.0.0.1 port (CC-21 path);
#   • loopback bridge — the in-process dial/send/recv bridge, hermetic, no host port (CC-33 path).
# Authority: the server's OWN response bytes ("HELLO-FROM-HOLO-REACHABLE") round-tripped through a real
# external client — the NAT cannot fake it.
#
# Witness: crates/holospaces/tests/cc60_x64_reachable_server.rs ::
#   a_real_amd64_server_image_is_reachable_from_the_host (host socket, #[ignore]),
#   a_real_amd64_server_image_is_reachable_over_the_loopback_bridge (loopback, #[ignore]).
#
# Depends on: CC-45 (x64 boot + κ-disk), CC-65 (image_init — the image's own entrypoint runs), the compiled
#   image-init template, virtio-net (eth0/NAT), StdIngress/poll_ingress. Heavy (boots a NIC'd server), so the
#   witnesses are #[ignore]d and run here explicitly.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc67-reachable-server: SKIP — cargo not available in this environment" >&2
    exit 127
fi
if [ ! -f "$ROOT/vv/artifacts/cc65/image-init" ]; then
    echo "cc67-reachable-server: SKIP — vv/artifacts/cc65/image-init not compiled (see image-init.c header)" >&2
    exit 127
fi

# Run the loopback-bridge witness (hermetic, deterministic) as the gate; the host-socket witness is the
# stronger real-external-client proof, runnable on demand (it binds a host port).
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --test cc60_x64_reachable_server \
    a_real_amd64_server_image_is_reachable_over_the_loopback_bridge -- --ignored --nocapture || exit 1

echo "cc67-reachable-server: PASS"
