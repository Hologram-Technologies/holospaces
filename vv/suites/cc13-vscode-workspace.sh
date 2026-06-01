#!/usr/bin/env bash
#
# CC-13 — vscode workspace (ADR-010)
#
# Component conformance suite (arc42 chapter 10, Conformance catalog). Witness:
# crates/holospaces/tests/cc13_vscode_workspace.rs. The full browser surface is witnessed by the
# Platform Manager / workspace browser tests (scripts/browser-manager-test.sh,
# the CI browser job).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc13-vscode-workspace: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc13_vscode_workspace -- --nocapture
