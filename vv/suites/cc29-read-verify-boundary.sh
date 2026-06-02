#!/usr/bin/env bash
#
# CC-29 — L5 verification is placed at the trust boundary, not charged as a
# per-read tax on the deployed peer (ADR-019)
#
# Law L5 accepts content only when its bytes re-derive to the requested κ. The
# substrate verifies ON RECEIPT at an untrusted gateway (get_with_fetch; the real
# loopback HTTP-CAS gateway used by the CC-28 / e2e witnesses) and the OCI
# ingestor verifies every blob against its sha256 digest on the way in (CC-10,
# verify_kappa_axis). Once content is in THIS peer's own in-session store, the
# store IS the canonical memory and RAM is its cache (Law L3): re-deriving it on
# every local read would treat the canonical store as untrusted and is pure
# overhead in the deployed browser peer. The deployed Console reads with
# ReadVerify::Trusted; a general/boundary caller reads with ReadVerify::OnRead.
#
# Authority/oracle: the substrate's own `verify_kappa` re-derivation contract —
# the exact primitive get_with_fetch and the OCI ingestor run at the receipt
# boundary. The threat is modelled in miniature by a forging store that lies
# about one κ (returns bytes that do not re-derive to it), as an untrusted
# gateway can.
#
# Witness: crates/holospaces/tests/cc29_read_verify_boundary.rs —
#   * the boundary check (OnRead) rejects the liar (L5 holds at the boundary);
#   * the trusted in-session read (Trusted) returns the store's bytes without
#     re-deriving (Law L3) — the deployed peer's read;
#   * honest content reads identically under both policies;
#   * resolve_local defaults to the verifying boundary policy.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc29-read-verify-boundary: SKIP — cargo unavailable" >&2; exit 127; fi
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc29_read_verify_boundary -- --nocapture || exit 1
