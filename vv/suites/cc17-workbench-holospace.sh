#!/usr/bin/env bash
#
# CC-17 (Phase 3) — the real VS Code web workbench, bound to the running
# holospace, served statically (no server) (ADR-012/ADR-015)
#
# holospaces is the substrate gateway: it serves Microsoft's κ-verified vscode-web
# executable core BYTE-IDENTICAL (Law L5) and COMPOSES it as its own content with
# VS Code's supported web `create()` bootstrap + the `holospace-fs` builtin
# extension, which boots the holospace in the extension-host worker (the browser
# is a first-class compute substrate) and binds the workbench to its virtio-9p
# workspace (CC-15) + console (CC-11), wired to the open gallery (Open VSX, CC-19).
# This is the architecture's Workspace Projection — the real workbench, not the
# CC-13 Monaco fallback. The witness composes the workbench exactly as the deploy
# does (build-workbench.mjs) and asserts it loads κ-verified + the LIVE holospace
# workspace mounts into the editor. Authority: the real vscode-web build + its
# FileSystemProvider/terminal APIs; the holospace primitives (CC-15/CC-11).
# Witness: crates/holospaces-web/web/vscode-workbench-holospace-test.mjs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"

if ! command -v node >/dev/null 2>&1; then echo "cc17-workbench-holospace: SKIP — node unavailable" >&2; exit 127; fi
# The wasm peer the holospace-fs extension boots in the workbench's extension host.
if [ ! -f "$WEB/pkg/holospaces_web_bg.wasm" ]; then
  "$ROOT/vv/lib/build-wasm-peer.sh" "$ROOT" || exit 1
fi
# A real witness installs its prerequisites — it does not skip.
[ -d "$WEB/node_modules/playwright" ] || ( cd "$WEB" && npm install playwright >/dev/null 2>&1 )
( cd "$WEB" && npx --yes playwright install chromium chromium-headless-shell >/dev/null 2>&1 ) || exit 1

node "$WEB/vscode-workbench-holospace-test.mjs" || exit 1
