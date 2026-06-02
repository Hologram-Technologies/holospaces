#!/usr/bin/env bash
#
# CC-17 — entering a holospace opens the real VS Code web workbench (ADR-012/015)
#
# Phase 1: the workspace is the REAL VS Code web workbench — the same compilation
# that powers vscode.dev / github.dev — not a reconstruction from Monaco + xterm
# (that is CC-13). The browser peer verifies the workbench's executable core by
# re-derivation against the committed manifest (Law L5) before loading, then the
# authentic workbench boots to its UI in the tab. Authority + pin:
# vscode-web@1.91.1 and vv/artifacts/cc17/{SOURCE.txt,vendor.sha256}.
# Witness: crates/holospaces-web/web/vscode-workbench-test.mjs (Chromium).
#
# Phase 2 (a FileSystemProvider over the virtio-9p workspace + the terminal) and
# Phase 3 (the remote extension host in the devcontainer OS, ADR-015) follow.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"

if ! command -v node >/dev/null 2>&1; then
    echo "cc17-vscode-workbench: SKIP — node not available in this environment" >&2
    exit 127
fi
cd "$WEB"
[ -d node_modules/playwright ] || npm install playwright >/dev/null 2>&1
# A real witness installs its prerequisites — it does not skip. Ensure the
# Playwright browser binaries are present (chromium AND chrome-headless-shell, the
# headless launch shell — a separate download), pinned to the installed Playwright.
npx --yes playwright install chromium chromium-headless-shell >/dev/null 2>&1 || exit 1
node vscode-workbench-test.mjs || exit 1
