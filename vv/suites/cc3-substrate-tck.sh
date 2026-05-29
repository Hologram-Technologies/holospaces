#!/usr/bin/env bash
#
# CC-3 — a peer's storage obeys the substrate contract (hologram_substrate_tck::store_battery)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). Validates against the imported external authority (provenance in
# vv/PROVENANCE.md). Witness: crates/holospaces/tests/cc3_substrate_tck.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc3-substrate-tck: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --test cc3_substrate_tck -- --nocapture
