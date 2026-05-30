#!/usr/bin/env bash
#
# CC-7 — the κ-disk preserves a real filesystem (the Linux ext4 on-disk format,
#        via e2fsprogs; ADR-009)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). The authority is a real ext4 image produced by e2fsprogs
# (vv/artifacts/cc7/, provenance in SOURCE.txt); the witness rounds it through a
# KappaStore-backed BlockDevice and reads the files back with debugfs.
# Witness: crates/holospaces/tests/cc7_kdisk.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc7-kdisk: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc7_kdisk -- --nocapture
