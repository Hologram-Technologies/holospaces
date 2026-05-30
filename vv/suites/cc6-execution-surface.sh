#!/usr/bin/env bash
#
# CC-6 — a recompiled userland runs on the execution surface (WebAssembly spec +
#        hologram ContainerRuntime contract; ADR-008, resolving RT1)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). Validates against the imported external authorities (provenance in
# vv/PROVENANCE.md). Witness: crates/holospaces/tests/cc6_execution_surface.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc6-execution-surface: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc6_execution_surface -- --nocapture
