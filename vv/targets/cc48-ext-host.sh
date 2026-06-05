#!/usr/bin/env bash
#
# CC-48 (TARGET) — The extension host runs an arbitrary WORKSPACE/Node marketplace
#                  extension IN THE x86-64 GUEST (Codespaces/Gitpod parity)
#
# OPM process: SD4 Working. Closes the CC-19 / CC-34 / ADR-020 frontier.
#
# THE BAR (non-negotiable — a prior attempt relabeled CC-19 with a web extension
# and was rejected): the x86-64 implementation MUST RUN THE EXTENSIONS. The real
# ecosystem extension host — `openvscode-server` (Node) — runs INSIDE the booted
# x86-64 devcontainer guest, hosts an arbitrary stock WORKSPACE/Node extension,
# and the workbench reaches it over the CC-33 bridge (CC-34). The experience must
# match Codespaces/Gitpod. We are NOT relying on vscode-web's WEB extension host.
#
# EXPLICITLY NOT ACCEPTABLE (legacy / relabel):
#   - Loading a web-capable extension via `additionalBuiltinExtensions` into VS
#     Code's web extension host — that is CC-19 (already live), not CC-48.
#   - A witness extension that declares a `browser` entrypoint. The subject MUST
#     be Node-only: package.json has `main` and NO `browser` (verify first), so it
#     cannot run in the web ext host — proving the x86-64-guest ext host does the
#     work.
#   - A separate vscode-server / any host process outside the holospace.
#
# Implement THROUGH the substrate, never a legacy fallback. Depends on CC-44 (#12,
# x86-64 Linux boot) + CC-46 (x86-64 shared-devbus transport, done) + CC-45 (amd64
# devcontainer). The guest-throughput "blocker" (a software interpreter is too slow
# for Node/V8) is overcome USING HOLOGRAM/THE SUBSTRATE (a fast execution path for
# the x86-64 guest on the substrate's wasm engine — JIT/AOT), never by reverting to
# vscode-web and never a ceiling report.
#
# Authority: the VS Code remote-server protocol + extension API; an arbitrary stock
#   Node-only Open VSX extension as the unmodified subject.
# Witness: crates/holospaces-web/web/ext-host-test.mjs — drives the workbench
#   against the x86-64-guest `openvscode-server` over the bridge; a Node-only
#   extension installs from Open VSX and activate()s in the guest ext host, its
#   contribution observable in the real workbench DOM.
#
# GREEN when: a Node-only (no `browser` entrypoint) Open VSX extension activates in
#   the `openvscode-server` running inside the booted x86-64 guest, over the bridge
#   — no vscode-web, no Node on the host, no deployment outside the holospace.
#
# Status: TARGET — not yet live. Expected RED (non-gating). Blocked on CC-44 (#12).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WITNESS="$ROOT/crates/holospaces-web/web/ext-host-test.mjs"
X64="$ROOT/crates/holospaces/src/emulator/x64.rs"

# Liveness probe: the x86-64 core boots Linux (CC-44) AND the witness drives the
# in-guest openvscode-server path (NOT the additionalBuiltinExtensions web path).
if grep -q 'fn boot_linux' "$X64" 2>/dev/null \
   && [ -f "$WITNESS" ] && grep -q 'openvscode-server' "$WITNESS" 2>/dev/null \
   && ! grep -qE 'additionalBuiltinExtensions.*EXT|extensions: \[EXT\]' "$WITNESS" 2>/dev/null; then
    command -v node >/dev/null 2>&1 || { echo "cc48-ext-host: SKIP — node absent"; exit 127; }
    ( cd "$ROOT/crates/holospaces-web/web" && node ext-host-test.mjs ) || exit 1
    exit 0
fi

echo "cc48-ext-host: RED — TARGET not yet live."
echo "  needed: the x86-64 guest (CC-44/#12) runs openvscode-server hosting a stock"
echo "          Node-only extension, reached over the bridge — NOT vscode-web."
echo "  spec:   a Node-only (no 'browser' entrypoint) Open VSX extension activates in"
echo "          the in-guest openvscode-server; throughput overcome via the substrate."
exit 1
