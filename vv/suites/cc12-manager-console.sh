#!/usr/bin/env bash
#
# CC-12 — manager console (ADR-010)
#
# Component conformance suite (arc42 chapter 10, Conformance catalog). Witness:
# crates/holospaces/tests/cc12_manager_console.rs. The full browser surface is witnessed by the
# Platform Manager / workspace browser tests (scripts/browser-manager-test.sh,
# the CI browser job).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc12-manager-console: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc12_manager_console -- --nocapture
