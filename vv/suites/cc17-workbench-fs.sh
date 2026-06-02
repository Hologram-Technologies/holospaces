#!/usr/bin/env bash
#
# CC-17 (Phase 2) — the real VS Code web workbench is served the holospace's
# workspace, the SUPPORTED way (ADR-012/015).
#
# github.dev / vscode.dev / Codespaces serve the browser workbench a workspace
# through a FileSystemProvider; holospaces uses that same mechanism via
# Microsoft's own @vscode/test-web (its built-in FileSystemProvider mounts the
# workspace — `--browser none` serves it; we drive our own Chromium). The real
# workbench loads and its readDirectory reaches the served workspace — no custom
# embedder, no service worker, no hand-rolled extension. The editor↔workspace
# CONTENT path (content by κ) is witnessed separately against the real OS over
# 9p by tests/cc17_workspace_fs.rs.
# Witness: crates/holospaces-web/web/vscode-workbench-fs-test.mjs (Chromium).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"

if ! command -v node >/dev/null 2>&1; then
    echo "cc17-workbench-fs: SKIP — node not available" >&2; exit 127
fi
cd "$WEB"
[ -d node_modules/playwright ] || npm install playwright >/dev/null 2>&1
# A real witness installs its prerequisites — it does not skip.
npx --yes playwright install chromium chromium-headless-shell >/dev/null 2>&1 || exit 1
[ -d node_modules/@vscode/test-web ] || npm install --no-save @vscode/test-web@0.0.80 >/dev/null 2>&1
node vscode-workbench-fs-test.mjs || exit 1
