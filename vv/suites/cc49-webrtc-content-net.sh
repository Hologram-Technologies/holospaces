#!/usr/bin/env bash
#
# CC-49 — Two browser peers exchange κ-content over a real WebRTC data channel
#
# OPM process: SD3 Sync ("Sync requires Identity + Substrate; affects Holospace").
# CC-38 proved the content-network PROTOCOL (BareNetSync verify-on-receipt) is
# portable and identical across native / wasm / bare-metal, with an in-test pump
# standing in for the link transport. This suite closes the named frontier in
# CC-38 / ADR-006: the SURFACE-SPECIFIC transport between two browser peers — a
# real RTCDataChannel — carries the same frames, peer-to-peer, with NO central
# operator (Law L1, UOR-native: no server).
#
# Authority: the substrate's content-addressed network (BareNetSync) over the
#   portable NetworkInterface (content_net::PacketLink / ContentPeer), carried by
#   a real WebRTC RTCDataChannel (holospaces-web::WebRtcLink — the pump); verify-
#   on-receipt (a forging responder rejected; an unheld κ resolves to nothing).
# Witness: crates/holospaces-web/web/webrtc-content-net-test.mjs — two browser
#   contexts connect a real RTCDataChannel (out-of-band SDP/ICE signaling, no
#   server) and one peer fetches a κ from the other, verified by re-derivation;
#   a forging responder is rejected; an unheld κ is absent; the exchange is
#   symmetric (either direction).
#
# GREEN when: a browser peer fetches content-addressed bytes from another browser
#   peer over a real RTCDataChannel, accepted only after re-derivation.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"

if ! command -v node >/dev/null 2>&1; then echo "cc49-webrtc-content-net: SKIP — node unavailable" >&2; exit 127; fi
if ! command -v wasm-pack >/dev/null 2>&1; then echo "cc49-webrtc-content-net: SKIP — wasm-pack unavailable" >&2; exit 127; fi

# The wasm peer carrying the content-network seam + the WebRTC pump (WebRtcLink).
if [ ! -f "$WEB/pkg/holospaces_web_bg.wasm" ] || ! grep -q "WebRtcLink" "$WEB/pkg/holospaces_web.js" 2>/dev/null; then
  ( cd "$ROOT/crates/holospaces-web" && wasm-pack build --release --target web --out-dir web/pkg ) || exit 1
fi
# A real witness installs its prerequisites — it does not skip.
[ -d "$WEB/node_modules/playwright" ] || ( cd "$WEB" && npm install playwright >/dev/null 2>&1 )
( cd "$WEB" && npx --yes playwright install chromium chromium-headless-shell >/dev/null 2>&1 ) || exit 1

node "$WEB/webrtc-content-net-test.mjs" || exit 1
