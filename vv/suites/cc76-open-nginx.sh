#!/usr/bin/env bash
# CC-76 (Polish-2) — an UNMODIFIED nginx:alpine opened live in a headless-Chromium tab (serverless).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"; CRATE="$ROOT/crates/holospaces-web"
if ! command -v wasm-pack >/dev/null || ! command -v node >/dev/null; then echo "cc76-open-nginx: SKIP — no wasm-pack/node" >&2; exit 127; fi
if [ ! -f "$CRATE/web/fixtures/x64-nginx.holo" ]; then echo "cc76-open-nginx: SKIP — fixtures/x64-nginx.holo missing (holo run nginx:alpine, copy the cached .holo)" >&2; exit 127; fi
( cd "$CRATE" && wasm-pack build --release --target web --out-dir web/pkg >/dev/null 2>&1 ) || exit 1
( cd "$CRATE/web" && npm install >/dev/null 2>&1; npx --yes playwright install chromium >/dev/null 2>&1; node open-nginx-test.mjs ) || exit 1
echo "cc76-open-nginx: PASS"
