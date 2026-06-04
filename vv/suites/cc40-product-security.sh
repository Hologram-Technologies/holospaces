#!/usr/bin/env bash
#
# CC-40 — product security: the threat model's properties are enforced, not
#         asserted (docs/13-Product-Security.md)
#
# Component conformance suite (arc42 ch.10 / ch.13). Each security requirement in
# the Product Security & Threat Model has a witness proving holospaces ENFORCES
# it. The properties are UOR-native — they hold by construction — so the
# witnesses assert what the substrate REFUSES:
#   * SEC-1 integrity      — a tampered byte fails re-derivation; the content
#                            network never fabricates content for an unheld κ.
#   * SEC-2 authority      — capabilities only attenuate; every escalation vector
#                            (an unheld flag, a wider quota, an unbounded budget
#                            under a bounded parent, a foreign root) is refused.
#   * SEC-3 cost/dedup     — identical content resolves to one κ, stored once.
#   * SEC-4 identity       — self-sovereign, deterministic, unforgeable.
#   * SEC-5 confidentiality— the κ is the capability to perceive; an unknown κ is
#                            absent (no enumeration, no fabrication).
# Witness: crates/holospaces/tests/cc40_product_security.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc40-product-security: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc40_product_security -- --nocapture || exit 1

echo "cc40-product-security: PASS"
