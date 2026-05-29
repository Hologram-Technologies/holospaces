#!/usr/bin/env bash
#
# CC-5 — Wasm code modules are specification-valid (WebAssembly spec; closed host surface)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). Validates against the imported external authority (provenance in
# vv/PROVENANCE.md). Witness: crates/holospaces/tests/cc5_wasm.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc5-wasm: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --test cc5_wasm -- --nocapture
