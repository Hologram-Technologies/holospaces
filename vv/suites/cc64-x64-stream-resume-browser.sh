#!/usr/bin/env bash
#
# CC-64 — one link → sub-second instant-paint streamed resume, in a REAL browser
#
# Component conformance suite (arc42 ch.10). The "open a link → a live Linux
# machine appears in a browser tab" promise needs the resume to be FAST and
# CHEAP-to-first-paint, not a 38 MiB stall. The warm snapshot's console already
# holds the `holo$` prompt, so first paint needs ZERO guest instructions and ZERO
# of the RAM: a tiny console "header" (~16 KiB) paints instantly, then the full
# machine streams in the background and the painted text is a byte-prefix of the
# live terminal (seamless swap). Additive — the whole-blob resume path
# (shell-worker.mjs) is untouched.
#
# Authority: a REAL headless Chromium (playwright) drives shell-stream.html and
#   measures, in-browser:
#     G1  first `holo$` paint  < 1000 ms wall-clock
#     G2  bytes transferred before that paint  < 2 MiB (proves streaming, not whole-blob)
#     G3  after the machine goes live, a typed command returns byte-exact output
#         (kernel line + HOLO-OK-42) — same correctness discipline as CC-62.
#
# Witness: crates/holospaces-web/web/x64-stream-resume-browser-test.mjs
#   (serves web/ with COOP/COEP, opens shell-stream.html, asserts G1/G2/G3).
#   Paint header: crates/holospaces-web/web/fixtures/x64-alpine-shell.console.txt
#   (generated from the warm kblob; the prompt the browser paints instantly).
#
# Depends on: CC-45 (warm-shell .holo generator), CC-59 (warm resume), the built
#   wasm pkg (crates/holospaces-web/web/pkg), node + playwright + chromium.

set -uo pipefail
WEB="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../crates/holospaces-web/web" && pwd)"

if ! command -v node >/dev/null 2>&1; then
    echo "cc64-x64-stream-resume-browser: SKIP — node not available in this environment" >&2
    exit 127
fi
if [ ! -f "$WEB/pkg/holospaces_web.js" ] || [ ! -f "$WEB/pkg/holospaces_web_bg.wasm" ]; then
    echo "cc64-x64-stream-resume-browser: SKIP — wasm pkg not built (run wasm-pack first)" >&2
    exit 127
fi
if [ ! -f "$WEB/fixtures/x64-alpine-shell.kblob" ] || [ ! -f "$WEB/fixtures/x64-alpine-shell.console.txt" ]; then
    echo "cc64-x64-stream-resume-browser: SKIP — warm .holo / console-header fixtures missing" >&2
    exit 127
fi
if [ ! -d "$WEB/node_modules/playwright" ]; then
    echo "cc64-x64-stream-resume-browser: SKIP — playwright not installed in web/node_modules" >&2
    exit 127
fi

node "$WEB/x64-stream-resume-browser-test.mjs" || exit 1

echo "cc64-x64-stream-resume-browser: PASS"
