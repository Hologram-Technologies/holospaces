#!/usr/bin/env bash
#
# CC-1 — κ-labels are correct content addresses.
#
# Component conformance suite for the Realizations / κ-addressing component
# (crates/holospaces/src/realizations.rs). Validates holospaces' κ-labels
# against the published σ-axis hash test vectors imported in
# vv/artifacts/cc1/hash-kats.json (external authority; provenance in
# vv/PROVENANCE.md), by re-derivation (Law L5). Defined by arc42 chapter 10
# (the Conformance catalog, row CC-1).
#
# The witness is the executable test crates/holospaces/tests/cc1_kappa_kat.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "CC-1: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --test cc1_kappa_kat -- --nocapture
