#!/usr/bin/env bash
#
# CC-15 — the editor and the running OS share the workspace filesystem
#         (the VirtIO 9P device + the 9P2000.L protocol; ADR-011)
#
# Component conformance suite (arc42 ch.10). holospaces serves a shared workspace
# over a spec-conformant virtio-9p device; the guest OS mounts it over virtio-9p
# and reads/writes the SAME files holospaces holds — a file holospaces places on
# the share is read by the OS, and a file the OS writes is read back by
# holospaces (one content, Law L1). Authority: the 9P2000.L protocol; the
# differential oracle is qemu-system-riscv64's own 9p server.
# Witness: crates/holospaces/tests/cc15_workspace.rs (release; a real-OS boot, ~20s).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc15-virtio-9p: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc15_workspace -- --ignored --nocapture \
    the_os_and_holospaces_share_the_workspace_over_virtio_9p || exit 1
