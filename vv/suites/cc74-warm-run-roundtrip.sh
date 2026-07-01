#!/usr/bin/env bash
#
# CC-74 — boot once, resume forever: a REAL image serves, is warm-snapshotted, and resumes SERVING
#
# Component conformance suite (arc42 ch.10). The witness behind the `holo run` warm-snapshot cache and the
# "heavy images run in practical time" promise: a live-pulled image (nginx:alpine) boots with a NIC, an
# external host client gets its REAL HTTP response (cold), the running machine is snapshotted
# (snapshot_kappa_blob — now carrying virtio-net device state), and a FRESH machine restored from that
# .holo with a RE-ATTACHED forward serves the REAL response AGAIN (warm), warm first-byte ≪ cold boot.
#
# Depends on: CC-65 (image_init / net-up-in-init), CC-73 (virtio-net κ-snapshot + reattach_net_forward),
#   a live registry (network). Heavy (boots a real image) → the witness is #[ignore]d and run here.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc74-warm-run-roundtrip: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --test cc74_warm_run_roundtrip \
    a_live_image_serves_then_warm_resumes_and_serves_again -- --ignored --nocapture || exit 1

echo "cc74-warm-run-roundtrip: PASS"
