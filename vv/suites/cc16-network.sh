#!/usr/bin/env bash
#
# CC-16 — the running OS reaches the internet through holospaces
#         (the VirtIO network device + a userspace TCP/IP NAT; ADR-014)
#
# Component conformance suite (arc42 ch.10). A devcontainer is not a dev
# environment if it can't git clone / apt-get / npm install from the internet.
# The guest OS drives a spec-conformant virtio-net device whose frames are
# terminated by a userspace TCP/IP NAT (ARP + DHCP so the guest gets an address;
# the guest-facing TCP state machine), and whose TCP streams are carried out over
# a pluggable egress transport — a direct host socket natively, a WebSocket
# tunnel to a relay in the browser (no raw NIC in a tab). The guest does DHCP,
# opens a TCP connection, and completes an HTTP exchange through the NAT to a real
# host server. Authorities: the OASIS VirtIO v1.2 spec (virtio-net) and the
# TCP/IP standards (RFC 826/791/768/2131/9293); the differential oracle is
# qemu-system-riscv64's own user-mode (slirp) network.
# Witness: crates/holospaces/tests/cc16_network.rs (release; a real-OS boot, ~21s).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc16-network: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# The NAT itself (ARP + DHCP framing + the TCP state machine) is unit-tested in
# the library against the protocol authorities — fast, no boot required.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --lib net:: || exit 1

# The full chain: a real Linux boots on the emulator, does DHCP over virtio-net,
# and completes an HTTP exchange through the NAT over the native egress.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc16_network -- --ignored --nocapture \
    the_os_reaches_the_internet_through_the_userspace_nat || exit 1
