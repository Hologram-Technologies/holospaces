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
#
# ── Measured environment ceiling (2026-06-05, recorded, not guessed) ──────────
# Two independent blockers were measured on the substrate as it stands; both are
# real and both must be lifted before this target can go GREEN for real (no
# fake-green, no narrowing — AGENTS.md).
#
# (A) Interpreter throughput. The riscv64 emulator core sustains ~11 Minsn/s of
#     active compute (measured: 2.0e9 retired instructions in 180.9 s on an AMD
#     EPYC 7763, release build; the CC-18 in-OS LSP server — a ~1 MB native
#     static binary — boots + serves a full session in 15.4 s). The stock
#     openvscode-server is Node.js/V8: bare Node startup alone is ~1.5e9 native
#     instructions (~136 s in-guest at this rate); binding :8000 with the bundled
#     server JS is conservatively ~1.5e10 (~23 min); the remote-agent handshake +
#     a second V8 ext-host fork + JIT-activating an extension push the total to
#     ~3e10+ (~45 min+), before browser-side drive. A headless witness cannot run
#     that in any practical CI budget. (A lower bound: ignores I/O stalls, GC, and
#     paging a ~120-180 MB server rootfs.)
#
# (B) Architecture reach. #16 mandates the stock linux-{arm64,amd64} server via
#     the CC-37 path (no riscv64 workaround). But the CC-33 bridge (dial_guest /
#     loopback), virtio-9p (CC-15), and virtio-net (CC-16) are riscv64-only today;
#     the AArch64 core (Aarch64Workspace) exposes terminal I/O ONLY — no bridge to
#     reach an in-guest server over. The mandated arm64 path therefore has no
#     transport to the remote yet. (x64 is #12, not required here.)
#
# Lifting the ceiling needs EITHER a JIT/AOT fast core (so Node/V8 boots in
# seconds, not tens of minutes) OR the bridge+9p+net ported to the AArch64 core
# (so a stock arm64 server is reachable) — tracked, not faked. Until then this
# stays a non-gating RED target; CC-48 is NOT flipped to live.

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
