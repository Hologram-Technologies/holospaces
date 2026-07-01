#!/usr/bin/env bash
# CC-76 (Polish-1) — open(κ) via a REAL content-addressed κ-URL: fetch the manifest + every unique page BY κ
# (L5-verified in wasm), resume, and serve the app in a headless-Chromium tab, 100% serverless.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; CRATE="$ROOT/crates/holospaces-web"
if ! command -v wasm-pack >/dev/null || ! command -v node >/dev/null; then echo "cc76-open-kappa-stream: SKIP — no wasm-pack/node" >&2; exit 127; fi
if [ ! -f "$CRATE/web/fixtures/store/.manifest-kappa" ]; then
  echo "cc76-open-kappa-stream: SKIP — κ-store missing (run: cargo test -p holospaces --release --features net --test cc76_browser_loopback_resume generate_browser_server_fixture -- --ignored)" >&2; exit 127; fi
( cd "$CRATE" && wasm-pack build --release --target web --out-dir web/pkg >/dev/null 2>&1 ) || { echo "cc76-open-kappa-stream: wasm build failed" >&2; exit 1; }
( cd "$CRATE/web" && npm install >/dev/null 2>&1; npx --yes playwright install chromium >/dev/null 2>&1; node open-kappa-stream-test.mjs ) || exit 1
echo "cc76-open-kappa-stream: PASS"
