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

# The workspace fixture: a real RISC-V Linux kernel + its device tree (the pinned
# CC-11 interactive image), the same bytes the Pages deploy ships. The browser
# peer boots it on the system emulator (CC-9 / CC-11).
cp "$ROOT/vv/artifacts/cc11/Image.gz" "$CRATE/web/workspace-kernel.gz"
cp "$ROOT/vv/artifacts/cc9/linux/holospaces.dtb" "$CRATE/web/workspace.dtb"

cd "$CRATE/web"
[ -d node_modules/playwright ] || npm install playwright >/dev/null 2>&1
if [ ! -d "$HOME/.cache/ms-playwright" ]; then
  echo "SKIP: Playwright browser not installed (run: npx playwright install chromium)"
  exit 0
fi

echo "==> running the Platform Manager test in Chromium"
node manager-test.mjs

echo "==> running the workspace test (boots a real Linux kernel in the browser)"
node workspace-test.mjs
