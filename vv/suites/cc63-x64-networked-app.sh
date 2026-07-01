#!/usr/bin/env bash
#
# CC-63 — a real NETWORKED application runs correctly on the x86-64 .holo core
#
# Component conformance suite (arc42 ch.10). The "run any docker image" promise
# needs more than correct CLI execution (CC-62) — most real images are servers.
# This is the standing gate for the socket/TCP application surface: a real TCP
# server (busybox nc -l) serves a real HTTP/1.0 response that a real HTTP client
# (busybox wget) connects to over the guest's loopback TCP/IP stack and parses;
# the body returns byte-exact. Exercises bind/listen/accept/connect/send/recv, the
# kernel TCP/IP + loopback path, fork/background, and HTTP framing — all on the
# warm Alpine .holo shell, no external NIC. A miscompute anywhere drops the
# connection or corrupts the body and fails the gate.
#
# Authority: the server's own bytes (the canned HTTP response) round-tripped
#   through the real client — a closed loop the emulator cannot fake without a
#   correct TCP stack. (Distinct from CC-60's loopback CONTROL-plane parity: this
#   drives a real guest server + client end-to-end.)
#
# Witness: crates/holospaces/tests/cc63_x64_networked_app.rs ::
#   x64_serves_a_real_http_round_trip_over_loopback (reports round-trip cost:
#   guest instructions + host wall-clock on the pure-Rust interpreter).
#
# Note: this busybox (v1.36.1) has no httpd applet, so nc is the server. The
#   external-NIC reachable-server (virtio-net + slirp) remains CC-60's RED target.
#
# Depends on: CC-44/45 (amd64 boot), CC-59 (warm-shell .holo fixture), CC-62.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc63-x64-networked-app: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc63_x64_networked_app -- --nocapture || exit 1

echo "cc63-x64-networked-app: PASS"
