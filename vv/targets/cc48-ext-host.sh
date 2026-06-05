#!/usr/bin/env bash
#
# CC-48 (TARGET) — The substrate-native extension host activates an arbitrary
#                  marketplace extension
#
# OPM process: SD4 Working (the projection runs the operator's chosen tools).
# This closes the open frontier named by CC-19 / CC-34 / ADR-020: holospaces is
# the VS Code remote (in the tab, on the substrate); the server-backed editor
# capabilities (LSP over the CC-33 bridge, CC-18) are already live, but the
# EXTENSION HOST that activates arbitrary marketplace (Open VSX) extensions —
# removing VS Code's "not available for the Web" notice — is not. holospaces
# provides that host on the hologram substrate (replacing the legacy Node
# vscode-server, Law L4), backed by the holospace's own filesystem (CC-15),
# terminal (CC-11), and network (CC-16) — never a server stood up elsewhere.
#
# Authority: the VS Code remote-server protocol + extension API; an arbitrary
#   workspace/Node extension from Open VSX as the unmodified subject.
# Witness: crates/holospaces-web/web/ext-host-test.mjs — an arbitrary Open VSX
#   extension activates against holospaces-as-remote and its contribution appears.
#
# GREEN when: a non-web (workspace/Node) marketplace extension activates in the
#   substrate-native extension host over the bridge — no Node server, no other
#   deployment.
#
# Status: TARGET — not yet live. Expected RED (non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WITNESS="$ROOT/crates/holospaces-web/web/ext-host-test.mjs"

if [ -f "$WITNESS" ]; then
    command -v node >/dev/null 2>&1 || { echo "cc48-ext-host: SKIP — node absent"; exit 127; }
    ( cd "$ROOT/crates/holospaces-web/web" && node ext-host-test.mjs ) || exit 1
    exit 0
fi

echo "cc48-ext-host: RED — TARGET not yet live."
echo "  needed: a substrate-native extension host (the holospace's own openvscode-server"
echo "          on the substrate, backed by CC-15/CC-11/CC-16); witness ext-host-test.mjs."
echo "  spec:   an arbitrary Open VSX workspace/Node extension activates against"
echo "          holospaces-as-remote over the bridge — no Node server, no other deployment."
exit 1
