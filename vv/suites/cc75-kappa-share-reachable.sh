#!/usr/bin/env bash
#
# CC-75 — share a running server BY κ: seal it into a content-addressed κ-snapshot, resume from that ONE
#         κ on a peer (store only), and the server is REACHABLE from the host.
#
# The bridge from `holo run` (a local warm .holo) to the north star "open a κ-link → a live reachable app"
# via the κ-manifest path: seal_kappa (content-address CPU+device state incl. virtio-net + per-page κ) →
# resume_kappa (fetch by κ, L5-verify manifest + every page) → reattach_net_forward → external round-trip.
#
# Depends on: CC-73 (virtio-net κ-snapshot + reattach), the κ-manifest resume path. Heavy → #[ignore]d.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then
    echo "cc75-kappa-share-reachable: SKIP — cargo not available in this environment" >&2
    exit 127
fi
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --test cc75_kappa_share_reachable a_server_sealed_by_kappa_resumes_reachable -- --ignored --nocapture || exit 1
echo "cc75-kappa-share-reachable: PASS"
