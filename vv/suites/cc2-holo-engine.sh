#!/usr/bin/env bash
#
# CC-2 — the .holo engine equals the native one (hologram-exec determinism + content-addressing + cross-surface agreement)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). Validates against the imported external authority (provenance in
# vv/PROVENANCE.md). Witness: crates/holospaces/tests/cc2_holo_engine.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc2-holo-engine: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --test cc2_holo_engine -- --nocapture
