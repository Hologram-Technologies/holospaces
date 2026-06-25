#!/usr/bin/env bash
#
# build-wasm-peer.sh — build the holospaces-web wasm peer into `web/pkg`,
# retrying the `wasm-pack build` to absorb a transient `wasm-opt` hiccup.
#
# The browser-conformance suites (CC-17/19/48/49/50/51) each need the wasm peer
# (`web/pkg/holospaces_web_bg.wasm`). wasm-pack's `-O3` `wasm-opt` post-pass on
# the ~5 MiB module is memory-heavy and, on a loaded CI runner, very occasionally
# exits non-zero — a transient failure, NOT a defect in the module: the pass is
# deterministic (the same input optimizes byte-identically run to run), so a
# retry succeeds. Centralizing the build here means one such hiccup never reds the
# whole V&V gate (it previously failed whichever suite happened to build the peer
# first). The build itself is unchanged — same command, same `-O3` flags (the
# real production post-pass, per crates/holospaces-web/Cargo.toml).
#
# Usage: build-wasm-peer.sh <repo-root>   (exit 0 = peer built/present; non-zero
#        = wasm-pack failed after the retries). The caller checks `wasm-pack`
#        availability and decides SKIP semantics.

set -uo pipefail
ROOT="${1:?usage: build-wasm-peer.sh <repo-root>}"
WASM="$ROOT/crates/holospaces-web/web/pkg/holospaces_web_bg.wasm"

attempts=3
for attempt in $(seq 1 "$attempts"); do
    if ( cd "$ROOT/crates/holospaces-web" \
            && wasm-pack build --release --target web --out-dir web/pkg ) \
        && [ -f "$WASM" ]; then
        exit 0
    fi
    # Only announce + back off when another attempt remains; the final failure
    # falls through to the error below without an extra "retrying" line or sleep.
    if [ "$attempt" -lt "$attempts" ]; then
        echo "build-wasm-peer: wasm-pack build attempt $attempt/$attempts failed (transient wasm-opt hiccup); retrying…" >&2
        rm -f "$WASM" 2>/dev/null
        sleep 3
    fi
done

echo "build-wasm-peer: wasm-pack build failed after $attempts attempts" >&2
exit 1
