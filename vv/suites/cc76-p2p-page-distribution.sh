#!/usr/bin/env bash
# CC-76 (Polish-3) — the open(κ) page store is served PEER-TO-PEER by κ (content_net), verified + tamper-refused.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc76-p2p-page-distribution: SKIP — no cargo" >&2; exit 127; fi
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net --test cc76_p2p_page_distribution || exit 1
echo "cc76-p2p-page-distribution: PASS"
