#!/usr/bin/env bash
#
# CC-39 — the holospaces node: the egress exit a browser tab routes through
#
# Component conformance suite (arc42 ch.10). A browser tab cannot open raw
# sockets, so a guest's arbitrary internet traffic (apt/pip/npm, a git clone, an
# outbound socket) leaves the tab through a holospaces node — a flashed
# low-powered device you own, the mesh's exit, NOT a bespoke external proxy. The
# browser already speaks the egress protocol (WsEgress, CC-16); the node is the
# peer it talks to.
#
# Witnessed:
#   * the egress protocol handler forwards a guest connection to a real host and
#     frames the reply back; an unreachable host reports FAILED (no silent drop).
#   * end-to-end over the REAL WebSocket transport the browser uses — a WebSocket
#     client opens a guest connection through the node to a real host and gets the
#     reply back (crates/holospaces-node/tests/ws_egress.rs).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc39-node-egress: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces-node -- --nocapture || exit 1

echo "cc39-node-egress: PASS"
