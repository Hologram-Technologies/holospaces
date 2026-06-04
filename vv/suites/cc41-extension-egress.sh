#!/usr/bin/env bash
#
# CC-41 — the local egress extension: a Chromebook guest reaches the internet
#         via a Chrome extension's Direct Sockets, with no node and no proxy
#
# Component conformance suite (arc42 ch.10 / ch.7). A browser tab cannot open a
# raw socket; an MV3 service worker can, via the Direct Sockets API
# (`TCPSocket`). The holospaces egress extension (`crates/holospaces-web/
# extension/`) is a LOCAL egress node in the browser: it speaks the SAME egress
# protocol the browser uses for a holospaces-node (OPEN/DATA/CLOSE, CC-16; the
# node implements it with std::net::TcpStream, CC-39) — only each guest
# connection is a TCPSocket. So a self-contained Chromebook's devcontainer gets
# arbitrary internet.
#
# Direct Sockets needs a real, gated Chrome to run for real, so the worker's
# behaviour is witnessed HERMETICALLY here: the service worker's logic is driven
# against a real echo server with `TCPSocket` polyfilled over node:net (faithful
# to the Direct Sockets contract) and `chrome.runtime` mocked — proving the
# extension is wire-compatible with WsEgress (CC-16) and the node (CC-39): OPEN
# opens a socket and reports OPENED, DATA round-trips, an unreachable host reports
# FAILED (no silent drop). Witness: crates/holospaces-web/extension/egress-test.mjs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v node >/dev/null 2>&1; then
    echo "cc41-extension-egress: SKIP — node not available in this environment" >&2
    exit 127
fi

node "$ROOT/crates/holospaces-web/extension/egress-test.mjs" || exit 1

# The extension must build into a publishable upload zip containing EXACTLY the
# runtime files (the build script validates the manifest is MV3 + minimal-
# permission and that no dev/test/page files leak into the artifact).
if command -v zip >/dev/null 2>&1 && command -v unzip >/dev/null 2>&1; then
    "$ROOT/scripts/build-extension.sh" || exit 1
else
    echo "cc41-extension-egress: zip/unzip absent — skipping the package build (logic witnessed above)" >&2
fi

echo "cc41-extension-egress: PASS"
