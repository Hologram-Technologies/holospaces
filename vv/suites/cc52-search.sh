#!/usr/bin/env bash
#
# CC-52 (LIVE) — The workbench's Search view finds and replaces across the REAL
#                workspace (find-in-files parity)
#
# OPM process: SD4 Working. The builtin `holospace-search` registers a
# FileSearchProvider + a TextSearchProvider (the proposed search APIs — a builtin
# keeps its enabledApiProposals) running as NATIVE exec on the browser peer (the
# CC-48 discipline, never the emulated guest) over the holospace's OWN virtio-9p
# workspace (CC-15): streamed results, include/exclude globs + `.gitignore`
# honored, and search & replace landing its edits over 9p (one content, Law L1).
# No server outside the holospace (Law L4). The web workbench has NO fallback
# search for a virtual scheme, so the provider is what makes find-in-files real.
#
# Authority: the VS Code FileSearchProvider/TextSearchProvider API, with the
#   workspace's actual file content as the authority a match re-derives against.
# Witnesses:
#   • core      builtin-extensions/holospace-search/search-core.test.cjs — the
#     engine (glob include/exclude, `.gitignore` via the vendored `ignore`, the
#     streaming/cancelling walk, and literal/regex/case/word/multiline matching
#     with re-deriving positions) verified deterministically under Node;
#   • deployed  crates/holospaces-web/web/search-test.mjs — in Chromium the
#     providers are live, a query returns the expected matches in the real Search
#     view, and replace-all edits the files (re-search confirms over 9p).
#
# GREEN when both pass: the search engine is correct (core) AND find-in-files +
#   replace work in the real deployed workbench (deployed).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"
SEARCH="$WEB/builtin-extensions/holospace-search"

# Artifact-drift gate: the vendored `ignore` (the `.gitignore` engine) must
# re-derive to its pinned sha256 (Law L5) — the extension verifies the same pin
# at load time.
( cd "$SEARCH/vendor/ignore" && sha256sum -c SHA256SUMS ) >/dev/null 2>&1 \
    || { echo "cc52-search: artifact drift in $SEARCH/vendor/ignore/SHA256SUMS" >&2; exit 1; }

command -v node >/dev/null 2>&1 || { echo "cc52-search: SKIP — node absent"; exit 127; }

# (1) Core witness — the search engine, fast + deterministic (no browser).
( cd "$SEARCH" && node search-core.test.cjs ) || exit 1

# (2) Deployed witness — find-in-files + replace in the real workbench (Chromium).
command -v wasm-pack >/dev/null 2>&1 || { echo "cc52-search: SKIP deployed witness — wasm-pack absent"; exit 127; }
# Build the wasm peer (holospace-fs boots it to provide the 9p workspace the
# search providers read) so the witness runs against the product.
if [ ! -f "$WEB/pkg/holospaces_web_bg.wasm" ]; then
    "$ROOT/vv/lib/build-wasm-peer.sh" "$ROOT" || exit 1
fi
( cd "$WEB" && node search-test.mjs ) || exit 1
exit 0
