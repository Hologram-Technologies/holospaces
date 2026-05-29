#!/usr/bin/env bash
#
# CC-4 — a devcontainer holospace matches its source (Dev Container + OCI; reproducible-κ)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). Validates against the imported external authority (provenance in
# vv/PROVENANCE.md). Witness: crates/holospaces/tests/cc4_devcontainer.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc4-devcontainer: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --test cc4_devcontainer -- --nocapture
