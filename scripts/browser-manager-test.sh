#!/usr/bin/env bash
# End-to-end Hologram Platform Manager test in a real browser: build the wasm32
# browser peer, generate the JS bindings (wasm-pack), and run the Playwright
# (Chromium) test — sign in · provision · view · resolve+verify (L5) · roster.
# Realizes the browser peer + Platform Manager served from GitHub Pages
# (arc42 ch.5 / ch.7). Requires wasm-pack, node, and Playwright's Chromium.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRATE="$ROOT/crates/holospaces-web"

if ! command -v wasm-pack >/dev/null || ! command -v node >/dev/null; then
  echo "SKIP: wasm-pack and/or node not available"
  exit 0
fi

echo "==> generating the .holo fixture (native executor → reference output κ)"
( cd "$ROOT" && cargo run -q -p holospaces --example holo_fixture -- "$CRATE/web" )

echo "==> building the browser peer (wasm32-unknown-unknown)"
wasm-pack build "$CRATE" --release --target web --out-dir web/pkg

# The workspace fixtures, the same bytes the Pages deploy ships and the browser
# imports by κ: the devcontainer OS (the pinned CC-11 Linux image + device tree;
# CC-9/CC-11) and the real VS Code components (Monaco + xterm.js; the pinned CC-13
# vendor set). The workspace verifies each by re-derivation before loading (L5).
cp "$ROOT/vv/artifacts/cc11/Image.gz" "$CRATE/web/workspace-kernel.gz"
cp "$ROOT/vv/artifacts/cc9/linux/holospaces.dtb" "$CRATE/web/workspace.dtb"
rm -rf "$CRATE/web/vendor"
cp -r "$ROOT/vv/artifacts/cc13/vendor" "$CRATE/web/vendor"

cd "$CRATE/web"
[ -d node_modules/playwright ] || npm install playwright >/dev/null 2>&1
if [ ! -d "$HOME/.cache/ms-playwright" ]; then
  echo "SKIP: Playwright browser not installed (run: npx playwright install chromium)"
  exit 0
fi

echo "==> running the Platform Manager console test in Chromium (CC-12)"
node manager-test.mjs

echo "==> running the VS Code workspace test in Chromium (CC-13: κ-verified Monaco + xterm.js, real OS)"
node workspace-test.mjs
