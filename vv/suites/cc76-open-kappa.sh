#!/usr/bin/env bash
# CC-76 — open(κ) in a REAL browser: a warm .holo resumes in a headless-Chromium tab and serves its app,
# 100% serverless. Builds the wasm (wasm-pack), then runs the Playwright witness over web/ with COOP/COEP.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CRATE="$ROOT/crates/holospaces-web"
if ! command -v wasm-pack >/dev/null || ! command -v node >/dev/null; then
  echo "cc76-open-kappa: SKIP — wasm-pack and/or node not available" >&2; exit 127; fi
if [ ! -f "$CRATE/web/fixtures/x64-server-loopback.holo" ]; then
  echo "cc76-open-kappa: SKIP — fixture missing (run: cargo test -p holospaces --release --features net --test cc76_browser_loopback_resume generate_browser_server_fixture -- --ignored)" >&2; exit 127; fi
( cd "$CRATE" && wasm-pack build --release --target web --out-dir web/pkg >/dev/null 2>&1 ) || { echo "cc76-open-kappa: wasm build failed" >&2; exit 1; }
( cd "$CRATE/web" && npm install >/dev/null 2>&1; npx --yes playwright install chromium >/dev/null 2>&1; node open-kappa-browser-test.mjs ) || exit 1
echo "cc76-open-kappa: PASS"
