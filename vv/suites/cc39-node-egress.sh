#!/usr/bin/env bash
#
# CC-39 — the holospaces node: a flashed bare-metal peer (egress + storage-sync
#         + OTA) a browser tab routes through
#
# Component conformance suite (arc42 ch.10). A browser tab cannot open raw
# sockets, has no durable storage, and cannot update a fleet. A holospaces node —
# a flashed low-powered device you own, the mesh's peer, NOT a bespoke external
# proxy — provides all three. The browser already speaks the egress protocol
# (WsEgress, CC-16); the node is the peer it talks to.
#
# Witnessed (crates/holospaces-node):
#   * EGRESS — the protocol handler forwards a guest connection to a real host and
#     frames the reply back; an unreachable host reports FAILED; the egress is
#     content-blind (an arbitrary binary payload is forwarded byte-identical —
#     SEC-7); and end-to-end over the REAL WebSocket the browser uses
#     (tests/ws_egress.rs).
#   * STORAGE-SYNC — content persists across a node restart, and the node serves
#     it over HTTP-CAS (GET /cas/{κ}), verified on receipt (CC-38/CC-20).
#   * OTA — the node fetches a κ-addressed update from the Pages site, verifies it
#     re-derives (Law L5), and stages it; a forged update is refused.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc39-node-egress: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces-node -- --nocapture || exit 1

echo "cc39-node-egress: PASS"
