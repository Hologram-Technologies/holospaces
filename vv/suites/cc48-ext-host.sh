#!/usr/bin/env bash
#
# CC-48 — The substrate-native extension host activates an arbitrary marketplace
#         extension (ADR-020)
#
# OPM process: SD4 Working (the projection runs the operator's chosen tools).
# Closes the frontier named by CC-19 / CC-34 / ADR-020: holospaces is the VS Code
# remote, in the tab, on the substrate. ADR-020's resolved answer for the ext host
# that activates ARBITRARY marketplace extensions is "holospaces' OWN, on the
# hologram substrate ... its VS Code + Node API surface backed by the holospace's
# own primitives" — i.e. the substrate's execution surface in the browser peer (the
# workbench's extension host, the same process the Workspace wasm peer runs in,
# ADR-015's web-model refinement), NOT Node booted inside the emulated guest (which
# measured ~11 Minsn/s → tens of minutes to boot V8, infeasible to witness; the
# evidence is recorded in git history). That host runs on the substrate peer with
# NO Node on the host and NO deployment outside the holospace (Law L4); its
# backends are the holospace's own filesystem (CC-15), terminal (CC-11), and
# language intelligence over the in-process substrate bridge (CC-18/CC-33).
#
# Authority: the real vscode-web build + the VS Code extension API; an arbitrary
#   workspace/Node extension from Open VSX (vscodevim.vim) as the unmodified
#   subject — a USER choice, never a holospaces default.
# Witness: crates/holospaces-web/web/ext-host-test.mjs — it composes the deployed
#   workbench EXACTLY as the deploy does (build-workbench.mjs), boots the holospace
#   in the ext host, declares the arbitrary extension so the launch installs it
#   from the open gallery, and asserts (1) holospaces-as-remote is live (the
#   substrate-native ext host runs, holospace-backed, surfacing HOLOSPACE-REMOTE-
#   LIVE), (2) the extension installs from Open VSX, (3) it ACTIVATES and its
#   contribution is observable in the real workbench DOM — all three executed.
#
# GREEN when: an arbitrary marketplace extension activates in the substrate-native
#   extension host, its contribution in the real workbench — no Node server, no
#   other deployment.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"

if ! command -v node >/dev/null 2>&1; then echo "cc48-ext-host: SKIP — node unavailable" >&2; exit 127; fi

# The wasm peer the holospace-fs extension boots in the workbench's extension host
# (the substrate execution surface). A real witness builds its prerequisites
# rather than skipping.
if [ ! -f "$WEB/pkg/holospaces_web_bg.wasm" ]; then
  command -v wasm-pack >/dev/null 2>&1 || { echo "cc48-ext-host: SKIP — wasm-pack unavailable" >&2; exit 127; }
  ( cd "$ROOT/crates/holospaces-web" && wasm-pack build --release --target web --out-dir web/pkg ) || exit 1
fi

[ -d "$WEB/node_modules/playwright" ] || ( cd "$WEB" && npm install playwright >/dev/null 2>&1 )
( cd "$WEB" && npx --yes playwright install chromium chromium-headless-shell >/dev/null 2>&1 ) || exit 1
[ -d "$WEB/node_modules/@vscode/test-web" ] || ( cd "$WEB" && npm install --no-save vscode-web@1.91.1 @vscode/test-web@0.0.80 >/dev/null 2>&1 )

node "$WEB/ext-host-test.mjs" || exit 1
