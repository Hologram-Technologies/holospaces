#!/usr/bin/env bash
#
# CC-8 — arbitrary code imported by κ runs over capability-scoped I/O (the
#        hologram driver-import + ContainerRuntime contract; ADR-009)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). A program is imported by κ and verified by re-derivation (a forged
# import is refused, Law L5), then run on the real Wasmtime runtime doing real
# storage_put/get bounded by its Capability Set (over-quota and out-of-roots I/O
# denied by the runtime). Witness: crates/holospaces/tests/cc8_import_run.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc8-import-run: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc8_import_run -- --nocapture
