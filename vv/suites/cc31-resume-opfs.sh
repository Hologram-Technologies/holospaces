#!/usr/bin/env bash
#
# CC-31 — A devcontainer resumes from a persisted κ snapshot over OPFS, in the browser
#
# Component conformance suite (arc42 ch.10). Witnessed in a real browser
# (Chromium/Playwright): `crates/holospaces-web/web/resume-test.mjs`.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"

if ! command -v node >/dev/null 2>&1; then
    echo "cc31-resume-opfs: SKIP — node not available in this environment" >&2
    exit 127
fi
cd "$WEB"
[ -d node_modules/playwright ] || npm install playwright >/dev/null 2>&1
npx --yes playwright install chromium chromium-headless-shell >/dev/null 2>&1 || exit 1
# The browser peer wasm must be built (pkg/). The Pages build / browser CI job
# builds it; locally, build it if absent.
[ -f pkg/holospaces_web.js ] || (cd "$ROOT" && wasm-pack build crates/holospaces-web --release --target web --out-dir web/pkg >/dev/null 2>&1) || exit 1
node resume-test.mjs || exit 1
