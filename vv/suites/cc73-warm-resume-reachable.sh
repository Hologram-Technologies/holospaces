#!/usr/bin/env bash
#
# CC-73 — a warm-snapshotted NIC'd server resumes and is REACHABLE from the host (self-contained .holo).
#
# The keystone under the `holo run` warm-snapshot cache: the κ-snapshot now serializes virtio-net device
# state, so a server booted with a NIC, snapshotted mid-listen, and resumed into a fresh Cpu with a
# re-attached forward is reached by a real external TcpStream. Depends on: CC-65, the virtio-net snapshot.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then
    echo "cc73-warm-resume-reachable: SKIP — cargo not available in this environment" >&2
    exit 127
fi
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --test cc73_warm_resume_reachable a_warm_snapshotted_server_resumes_and_is_reachable_from_the_host -- --ignored --nocapture || exit 1
echo "cc73-warm-resume-reachable: PASS"
