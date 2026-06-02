#!/usr/bin/env bash
#
# CC-19 (foundation) — a real extension runs in the real VS Code web workbench
# (ADR-012/015)
#
# Codespaces / vscode.dev run extensions in a real extension host; holospaces
# uses that mechanism via Microsoft's own @vscode/test-web
# (--extensionDevelopmentPath serves a real web extension to the real workbench
# with the proper extension-host wiring; --browser none serves, our own Chromium
# drives). The witness loads a real web extension and asserts it ACTIVATES in the
# genuine workbench (its status-bar contribution appears) — the extension host
# runs extensions in the holospaces workbench. That is the prerequisite for real
# extensions + their integrations (the GitHub sign-in → PRs/issues scenario) and
# language servers (CC-18). No hand-rolled embedder, no hacks.
# Witness: crates/holospaces-web/web/vscode-extension-test.mjs (Chromium).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"
if ! command -v node >/dev/null 2>&1; then echo "cc19-extensions: SKIP — node unavailable" >&2; exit 127; fi
cd "$WEB"
[ -d node_modules/playwright ] || npm install playwright >/dev/null 2>&1
# A real witness installs its prerequisites — it does not skip.
npx --yes playwright install chromium chromium-headless-shell >/dev/null 2>&1 || exit 1
[ -d node_modules/@vscode/test-web ] || npm install --no-save @vscode/test-web@0.0.80 >/dev/null 2>&1
node vscode-extension-test.mjs || exit 1
