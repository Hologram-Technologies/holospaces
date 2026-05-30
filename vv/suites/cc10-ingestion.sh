#!/usr/bin/env bash
#
# CC-10 — a devcontainer's OS image + repository ingest as κ content (the OCI
#         image + Dev Container specifications; ADR-009)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). A real OCI image-layout produced by BuildKit (vv/artifacts/cc10/,
# provenance in SOURCE.txt) is walked index→manifest→config+layers and every
# blob is verified by re-derivation against its OCI sha256 digest (Law L5); the
# repository's devcontainer.json (CC-4) binds to the image into a reproducible
# source identity. The booted *behaviour* of the image is the emulator's (CC-9).
# Witness: crates/holospaces/tests/cc10_ingestion.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc10-ingestion: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc10_ingestion -- --nocapture
