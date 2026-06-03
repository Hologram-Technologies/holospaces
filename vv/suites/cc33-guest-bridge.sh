#!/usr/bin/env bash
#
# CC-33 — a server inside the devcontainer is reachable from the workbench over
# the in-process substrate bridge (ADR-020)
#
# The browser peer's workbench extension host and the system emulator are one
# process, so reaching a server INSIDE the guest is an in-process ingress
# connection into the emulator's own userspace NAT (CC-16) — the inward dual of
# the egress relay, and the same NAT the native forwarded-port ingress (CC-21)
# drives. This is the transport the VS Code remote extension host runs over
# (ADR-015/ADR-020). The host dials a guest listener, writes the client's bytes
# and reads the guest's reply, byte-faithfully, with no relay or socket.
# Authority: TCP/IP + the guest's own socket; oracle: the 10.0.2.0/24 NAT model
# (qemu-system-riscv64 user networking). The CC-21 server image is the guest.
#
# Witnesses:
#   * the loopback transport's plumbing (a fast unit test, no OS boot);
#   * the end-to-end round trip over a real-OS boot (release; #[ignore]d).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc33-guest-bridge: SKIP — cargo unavailable" >&2; exit 127; fi
# The transport plumbing (fast).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --lib emulator::net::tests::the_loopback_bridge -- --nocapture || exit 1
# The end-to-end bridge round trip against a real guest server (release; boots an OS).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc33_guest_bridge -- --ignored --nocapture \
    a_guest_server_is_reachable_over_the_in_process_substrate_bridge || exit 1
