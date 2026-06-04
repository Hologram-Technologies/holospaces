#!/usr/bin/env bash
#
# CC-38 — the uor-native content network ("the browser as a router")
#
# Component conformance suite (arc42 ch.10). A peer fetches another peer's
# content the same content-addressed way on every deployment surface. The
# substrate supplies the mechanism (hologram-net-bare's BareNetSync over the
# NetworkInterface HAL); holospaces supplies a portable NetworkInterface
# (content_net::PacketLink), so the SAME sync runs in a browser tab (wasm), on a
# bare-metal board (thumbv7em-none-eabi), and on a native host — a browser peer
# and a bare-metal peer interoperate by construction.
#
# Witnessed two ways:
#   * the cross-peer exchange — two peers (browser + bare-metal representatives,
#     identical code) exchange content over the uor-native protocol with
#     verify-on-receipt (Law L5); a κ no peer holds resolves to nothing.
#     Witness: crates/holospaces/tests/cc38_content_net.rs.
#   * the COMPATIBILITY gate — the content network compiles for the bare-metal
#     target (thumbv7em-none-eabi, no_std) AND the browser target (wasm32), the
#     same code path, so the two implementations cannot diverge.
# (The browser surface drives the same path in Chromium —
# holospaces-web `Console::content_network_selftest`, in the manager test.)

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc38-content-net: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# The cross-peer content exchange (browser ↔ bare-metal, verify-on-receipt).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc38_content_net -- --nocapture || exit 1

# The compatibility gate: the IDENTICAL content network compiles for bare-metal
# (no_std) and the browser (wasm32). Same code → the peers cannot diverge.
rustup target add thumbv7em-none-eabi wasm32-unknown-unknown >/dev/null 2>&1 || true
cargo build --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --no-default-features --target thumbv7em-none-eabi || exit 1
cargo build --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --target wasm32-unknown-unknown || exit 1

echo "cc38-content-net: PASS"
