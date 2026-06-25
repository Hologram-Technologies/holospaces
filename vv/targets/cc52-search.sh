#!/usr/bin/env bash
#
# CC-52 (TARGET) — The workbench's Search view finds and replaces across the REAL
#                  workspace (find-in-files parity)
#
# OPM process: SD4 Working. `holospace-fs` registers no search provider, and the
# web workbench has NO fallback search for a virtual scheme (it dispatches
# per-scheme; a `holospace://` workspace with no provider returns nothing), so
# find-in-files is dead. This adds a builtin `holospace-search` that registers a
# FileSearchProvider + a TextSearchProvider running as NATIVE exec on the browser
# peer (the CC-48 discipline — never the emulated guest) over the holospace's OWN
# virtio-9p workspace (CC-15): a query streams matching files + lines with
# context, include/exclude globs and `.gitignore` are honored, and search &
# replace lands its edits in the 9p workspace — the same content the guest sees
# (one content, Law L1). No server outside the holospace (Law L4).
#
# Authority: the VS Code FileSearchProvider/TextSearchProvider API, with the
#   workspace's actual file content as the authority a match re-derives against
#   (a reported match must occur at its line/column in the file — Law L5).
# Witness: crates/holospaces-web/web/search-test.mjs — in Chromium a text query
#   over a real workspace returns the expected matches (file + line + preview),
#   a match in an excluded / `.gitignore`d path is absent, and replace-all edits
#   the files (read back over 9p); results arrive streamed.
#
# GREEN when: find-in-files returns correct results from the 9p workspace,
#   excludes/`.gitignore` are honored, replace-all applies (edits visible over
#   9p), and results stream — witnessed in the real deployed workbench.
#
# Status: TARGET — not yet live. Expected RED (non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SEARCH_EXT="$ROOT/crates/holospaces-web/web/builtin-extensions/holospace-search/extension.js"
WEB_WITNESS="$ROOT/crates/holospaces-web/web/search-test.mjs"

# Liveness probe: the holospace-search builtin exists AND the witness exists.
if [ -f "$SEARCH_EXT" ] && [ -f "$WEB_WITNESS" ]; then
    command -v node >/dev/null 2>&1 || { echo "cc52-search: SKIP — node absent"; exit 127; }
    command -v wasm-pack >/dev/null 2>&1 || { echo "cc52-search: SKIP — wasm-pack absent"; exit 127; }
    if [ ! -f "$ROOT/crates/holospaces-web/web/pkg/holospaces_web_bg.wasm" ]; then
        "$ROOT/vv/lib/build-wasm-peer.sh" "$ROOT" || exit 1
    fi
    ( cd "$ROOT/crates/holospaces-web/web" && node search-test.mjs ) || exit 1
    exit 0
fi

echo "cc52-search: RED — TARGET not yet live."
echo "  needed: a builtin holospace-search that registers a FileSearchProvider +"
echo "          TextSearchProvider (proposed APIs; a builtin keeps its"
echo "          enabledApiProposals) as native browser-peer exec over the 9p"
echo "          workspace (CC-15); honoring include/exclude globs + .gitignore;"
echo "          streaming results; and search & replace landing edits over 9p."
echo "  spec:   find-in-files returns correct results, excludes/.gitignore are"
echo "          honored, replace-all applies (visible over 9p), results stream —"
echo "          witnessed in the real workbench (search-test.mjs)."
exit 1
