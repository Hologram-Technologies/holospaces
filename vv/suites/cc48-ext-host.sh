#!/usr/bin/env bash
#
# CC-48 (LIVE) — The SUBSTRATE-NATIVE extension host activates an arbitrary
#                Node-only marketplace extension
#
# OPM process: SD4 Working. Closes the CC-19 / CC-34 / ADR-020 frontier.
#
# EXECUTION SURFACE (the hologram-substrate-native way — corrected v2):
# the extension host runs as NATIVE hologram exec on the browser peer's own wasm
# execution surface — a JS/Node-API runtime compiled to wasm32 (CpuBackend) — with
# residency handled by the tiered MemKappaStore -> OpfsKappaStore store
# (opfs_store.rs; cold-start via DevcontainerProvision/assembleIntoOpfs,
# warm-resume). It is reached over the CC-33 bridge (CC-34) and backed by the
# holospace's OWN filesystem (CC-15), terminal (CC-11), and network (CC-16).
# This is the same native-exec discipline the downstream in-browser inference uses
# (native wasm exec, NOT the system emulator), and what CC-34's narration already
# specified ("a substrate-native runtime for the ext host ... the VS Code + Node
# API surface backed by the holospace's own primitives").
#
# EXPLICITLY NOT ACCEPTABLE (the two anti-patterns):
#   1. openvscode-server (or any Node) running INSIDE the emulated x86-64 guest —
#      that is the "interpreter wall" (~tens of MIPS; un-JIT-able while the
#      workspace forbids unsafe). Heavy work never runs on the emulated CPU.
#   2. A `browser`-entrypoint extension loaded via additionalBuiltinExtensions into
#      vscode-web's WEB extension host — that is CC-19 (already live), not CC-48.
#      The subject MUST be Node-only: package.json has `main` and NO `browser`
#      (the witness verifies this against Open VSX before accepting it).
#   ... and never any Node on the host / a server / a deployment outside the holospace.
#
# Authority: the VS Code remote-server protocol + extension API; an arbitrary stock
#   Node-only Open VSX extension as the unmodified subject.
# Witness: crates/holospaces-web/web/ext-host-test.mjs — drives the real deployed
#   workbench against the substrate-native (wasm-exec) ext host; a Node-only
#   extension installs from Open VSX and activate()s there, its contribution
#   observable in the real workbench DOM.
#
# GREEN when: a Node-only (no `browser` entrypoint) Open VSX extension activates in
#   the substrate-native (wasm-exec) extension host — no emulated-guest server, no
#   vscode-web web host, no Node on the host, no deployment outside the holospace.
#
# Status: LIVE (promoted to vv/suites/ — gated). A Node-only Open VSX extension
#   (editorconfig.editorconfig, committed + sha256-pinned in vv/artifacts/cc48)
#   genuinely activates in the substrate-native (wasm-exec) ext host, witnessed in
#   headless Chromium.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"
WITNESS="$WEB/ext-host-test.mjs"

# Run the witness only when it encodes the substrate-native, Node-only bar — never
# a web-ext relabel. The witness is authoritative: it self-checks its prerequisites
# (the wasm peer, the substrate-native ext-host runtime) and reports honest RED if
# the runtime is not yet built, rather than skip-passing.
if [ -f "$WITNESS" ] \
   && grep -q 'NODE-EXTHOST-LIVE' "$WITNESS" 2>/dev/null \
   && grep -qE "no .browser. entrypoint|isNodeOnly" "$WITNESS" 2>/dev/null \
   && ! grep -qE 'additionalBuiltinExtensions.*EXT|extensions: \[EXT\]' "$WITNESS" 2>/dev/null; then
    command -v node >/dev/null 2>&1 || { echo "cc48-ext-host: SKIP — node absent"; exit 127; }
    command -v wasm-pack >/dev/null 2>&1 || { echo "cc48-ext-host: SKIP — wasm-pack absent"; exit 127; }
    # Artifact-drift gate: the committed Open VSX .vsix the witness installs must
    # re-derive to its pinned sha256 (Law L5) — a tampered/updated fixture is refused.
    ( cd "$ROOT/vv/artifacts/cc48" && sha256sum -c cc48.sha256 ) >/dev/null 2>&1 \
        || { echo "cc48-ext-host: artifact drift from cc48.sha256" >&2; exit 1; }
    # Fast core gate: the substrate-native ext-host runtime (the CommonJS module
    # loader + Node API surface + vscode passthrough + activate harness) must load a
    # Node-only extension and run activate() — verified in Node, no browser needed.
    ( cd "$WEB/builtin-extensions/holospace-fs" && node node-exthost.test.cjs ) || exit 1
    # Build the wasm peer (carries the substrate-native ext-host runtime) so the
    # witness always runs against the product, not a stale bundle.
    if [ ! -f "$WEB/pkg/holospaces_web_bg.wasm" ]; then
        "$ROOT/vv/lib/build-wasm-peer.sh" "$ROOT" || exit 1
    fi
    ( cd "$WEB" && node ext-host-test.mjs ) || exit 1
    exit 0
fi

echo "cc48-ext-host: FAILED — the witness does not encode the substrate-native, Node-only bar,"
echo "  or a prerequisite is missing. ext-host-test.mjs must: assert NODE-EXTHOST-LIVE, gate on a"
echo "  Node-only subject (package.json main, NO browser), and NOT relabel a web extension."
echo "  runtime prereqs: node + wasm-pack (to build pkg/) + Chromium (Playwright) to drive it."
echo "  the host itself runs as native hologram exec on the browser peer's wasm exec surface,"
echo "  reached over the CC-33 bridge, backed by CC-15/CC-11/CC-16 — not the emulated guest."
exit 1
