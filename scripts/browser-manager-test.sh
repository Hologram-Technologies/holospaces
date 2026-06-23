#!/usr/bin/env bash
# End-to-end Hologram Platform Manager test in a real browser: build the wasm32
# browser peer, generate the JS bindings (wasm-pack), and run the Playwright
# (Chromium) test — sign in · provision · view · resolve+verify (L5) · roster.
# Realizes the browser peer + Platform Manager served from GitHub Pages
# (arc42 ch.5 / ch.7). Requires wasm-pack, node, and Playwright's Chromium.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRATE="$ROOT/crates/holospaces-web"

if ! command -v wasm-pack >/dev/null || ! command -v node >/dev/null; then
  echo "SKIP: wasm-pack and/or node not available"
  exit 0
fi

echo "==> generating the .holo fixture (native executor → reference output κ)"
( cd "$ROOT" && cargo run -q -p holospaces --example holo_fixture -- "$CRATE/web" )

echo "==> building the browser peer (wasm32-unknown-unknown)"
wasm-pack build "$CRATE" --release --target web --out-dir web/pkg

# The workspace fixtures, the same bytes the Pages deploy ships and the browser
# imports by κ: the devcontainer OS (the pinned CC-11 Linux image + device tree;
# CC-9/CC-11) and the real VS Code components (Monaco + xterm.js; the pinned CC-13
# vendor set). The workspace verifies each by re-derivation before loading (L5).
cp "$ROOT/vv/artifacts/cc11/Image.gz" "$CRATE/web/workspace-kernel.gz"
cp "$ROOT/vv/artifacts/cc9/linux/holospaces.dtb" "$CRATE/web/workspace.dtb"
rm -rf "$CRATE/web/vendor"
cp -r "$ROOT/vv/artifacts/cc13/vendor" "$CRATE/web/vendor"
# The devcontainer (CC-14/CC-20): the virtio-mmio kernel + the OCI image's layer,
# so the browser peer assembles the rootfs (the in-crate Layer Assembler, wasm)
# and boots it over virtio-blk — the devcontainer flow, in the browser.
cp "$ROOT/vv/artifacts/cc14/kernel/Image.gz" "$CRATE/web/devcontainer-kernel.gz"
mfdig=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['manifests'][0]['digest'].split(':')[1])" "$ROOT/vv/artifacts/cc14/image/index.json")
ldig=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['layers'][0]['digest'].split(':')[1])" "$ROOT/vv/artifacts/cc14/image/blobs/sha256/$mfdig")
cp "$ROOT/vv/artifacts/cc14/image/blobs/sha256/$ldig" "$CRATE/web/devcontainer-layer.tar.gz"
# The *deployed* devcontainer base (CC-22): the BusyBox layer the Pages deploy
# assembles into the persistent interactive shell (DEVCONTAINER_INIT needs its
# busybox + setsid/stty applets). The raw-terminal test boots this, as the deploy
# does — distinct from the init-only CC-14 layer the boot/resume tests use.
bmfdig=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['manifests'][0]['digest'].split(':')[1])" "$ROOT/vv/artifacts/cc22/image/index.json")
bldig=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['layers'][0]['digest'].split(':')[1])" "$ROOT/vv/artifacts/cc22/image/blobs/sha256/$bmfdig")
cp "$ROOT/vv/artifacts/cc22/image/blobs/sha256/$bldig" "$CRATE/web/devcontainer-busybox-layer.tar.gz"
# The deployed devcontainer layer (CC-18 over ADR-020): BusyBox + the lsp-demo
# language server. The deploy boots this on the CC-16 net kernel with the
# loopback bridge, so the workbench gets language intelligence from the in-OS
# server over the bridge (CC-33). The LSP browser test boots this layer.
lmfdig=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['manifests'][0]['digest'].split(':')[1])" "$ROOT/vv/artifacts/cc18/image/index.json")
lldig=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['layers'][0]['digest'].split(':')[1])" "$ROOT/vv/artifacts/cc18/image/blobs/sha256/$lmfdig")
cp "$ROOT/vv/artifacts/cc18/image/blobs/sha256/$lldig" "$CRATE/web/devcontainer-lsp-layer.tar.gz"
# The networked devcontainer (CC-16): the net-enabled kernel + the init layer, so
# the browser peer boots virtio-net + the userspace NAT and tunnels TCP out.
cp "$ROOT/vv/artifacts/cc16/kernel/Image.gz" "$CRATE/web/devcontainer-net-kernel.gz"
nmfdig=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['manifests'][0]['digest'].split(':')[1])" "$ROOT/vv/artifacts/cc16/image/index.json")
nldig=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['layers'][0]['digest'].split(':')[1])" "$ROOT/vv/artifacts/cc16/image/blobs/sha256/$nmfdig")
cp "$ROOT/vv/artifacts/cc16/image/blobs/sha256/$nldig" "$CRATE/web/devcontainer-net-layer.tar.gz"

cd "$CRATE/web"
# Install the declared browser-test dependencies (playwright, @vscode/test-web,
# vscode-web) in one go — declared in package.json so nothing is pruned by a
# later ad-hoc install. A real witness installs its prerequisites; it does not skip.
npm install >/dev/null 2>&1
npx --yes playwright install chromium chromium-headless-shell >/dev/null 2>&1

echo "==> running the Platform Manager console test in Chromium (CC-12)"
node manager-test.mjs

echo "==> running the VS Code workspace test in Chromium (CC-13: κ-verified Monaco + xterm.js, real OS)"
node workspace-test.mjs

echo "==> running the devcontainer boot test in Chromium (CC-14/CC-20: assemble OCI image + virtio-blk boot in the browser)"
node devcontainer-test.mjs

echo "==> running the devcontainer resume test in Chromium (CC-30/CC-31: suspend → κ snapshot → gzip → OPFS → reload → verify(L5) → resume, workspace intact)"
node resume-test.mjs

echo "==> running the multi-holospace snapshot-keying test (CC-31: each holospace's resume state is keyed by its identity κ — distinct holospaces never share an OPFS slot/bleed)"
node snapshot-keying-test.mjs

echo "==> running the raw terminal test in Chromium (CC-11: raw keystrokes echoed/edited by the guest tty, Ctrl-C interrupts, delta streaming)"
node terminal-test.mjs

echo "==> running the streaming-assembly boot test in Chromium (CC-50: the rootfs streamed sparse straight into an OPFS file BOOTS to userspace via the shipped paged-κ-disk path — the deployed assembleIntoOpfs path, not a byte-identity substitute)"
node cc50-streaming-boot-test.mjs

echo "==> running the DEPLOYED provision->boot test in Chromium (a registry image pulled via DevcontainerProvision, assembled SPARSE into OPFS via the deployed assembleIntoOpfs, and BOOTED to userspace — the exact deployed provisioning path no other witness exercised; the regression guard for 'the deploy can't boot a real image')"
node provision-boot-test.mjs

echo "==> running the LSP-over-bridge test in Chromium (CC-18 deployed / ADR-020: real language intelligence from a server in the devcontainer OS, over the in-process substrate bridge — no Node)"
node lsp-test.mjs

echo "==> running the devcontainer network test in Chromium (CC-16: virtio-net + userspace NAT, egress tunnelled over a WebSocket relay)"
node devcontainer-net-test.mjs

echo "==> running the VS Code workbench test in Chromium (CC-17 Phase 1: the real VS Code web workbench loads κ-verified)"
node vscode-workbench-test.mjs

echo "==> running the VS Code workbench FS test in Chromium (CC-17 Phase 2: the real workbench is served the holospace workspace, github.dev-style)"
node vscode-workbench-fs-test.mjs

echo "==> running the VS Code extension test in Chromium (CC-19 foundation: a real extension activates in the real workbench)"
node vscode-extension-test.mjs

echo "==> running the extensions marketplace test in Chromium (CC-19: the gallery displays + lists real extensions from Open VSX)"
# Compose the deployed workbench (workbench.html + dist + holospace-fs) and stage
# the peer assets so the real config is exercised, then drive the marketplace.
HS_SITE="$(mktemp -d)"
node build-workbench.mjs "$HS_SITE"
cp -r pkg "$HS_SITE/pkg"
cp index.html "$HS_SITE/" 2>/dev/null || true
cp devcontainer-kernel.gz devcontainer-layer.tar.gz "$HS_SITE/" 2>/dev/null || true
node marketplace-test.mjs "$HS_SITE"
rm -rf "$HS_SITE"

echo "==> running the devcontainer-extensions test in Chromium (CC-19: a devcontainer's declared web extensions auto-install from Open VSX)"
node extensions-test.mjs
