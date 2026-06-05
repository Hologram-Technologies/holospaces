#!/usr/bin/env bash
#
# CC-49 (TARGET) — Two browser peers exchange κ-content over a real WebRTC data channel
#
# OPM process: SD3 Sync ("Sync requires Identity + Substrate; affects Holospace").
# CC-38 proved the content-network PROTOCOL (BareNetSync verify-on-receipt) is
# portable and identical across native / wasm / bare-metal, with an in-test pump
# standing in for the link transport. This target closes the named frontier in
# CC-38 / ADR-006: the SURFACE-SPECIFIC transport between two browser tabs — a
# real RTCDataChannel — carries the same frames, peer-to-peer, no central
# operator (UOR-native: no server).
#
# Authority: the substrate's content-addressed network (BareNetSync) over the
#   portable NetworkInterface, carried by a real WebRTC RTCDataChannel; verify-
#   on-receipt (a forging responder rejected; an unheld κ resolves to nothing).
# Witness: crates/holospaces-web/web/webrtc-content-net-test.mjs — two browser
#   contexts connect an RTCDataChannel and one fetches a κ from the other.
#
# GREEN when: a browser peer fetches content-addressed bytes from another browser
#   peer over a real RTCDataChannel, verified by re-derivation before acceptance.
#
# Status: TARGET — not yet live. Expected RED (non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WITNESS="$ROOT/crates/holospaces-web/web/webrtc-content-net-test.mjs"
PACKETLINK="$ROOT/crates/holospaces-web/src"

# Liveness probe: a WebRTC-backed PacketLink transport exists AND the witness exists.
if grep -rqE 'RtcDataChannel|RTCDataChannel|webrtc' "$PACKETLINK" 2>/dev/null \
   && [ -f "$WITNESS" ]; then
    command -v node >/dev/null 2>&1 || { echo "cc49-webrtc-content-net: SKIP — node absent"; exit 127; }
    ( cd "$ROOT/crates/holospaces-web/web" && node webrtc-content-net-test.mjs ) || exit 1
    exit 0
fi

echo "cc49-webrtc-content-net: RED — TARGET not yet live."
echo "  needed: an RTCDataChannel-backed NetworkInterface (content_net::PacketLink) on the"
echo "          browser surface; witness webrtc-content-net-test.mjs."
echo "  spec:   two browser peers exchange κ-content over a real WebRTC data channel,"
echo "          verified on receipt (forging responder rejected; unheld κ absent)."
exit 1
