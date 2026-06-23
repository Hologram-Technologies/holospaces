#!/usr/bin/env bash
# CC-48 fixture — the unmodified Node-only marketplace extension the substrate-native
# extension-host witness activates. Downloaded from Open VSX (the open gallery, no
# Microsoft account/EULA), pinned by version + sha256 so the gated witness is
# hermetic and reproducible (imported external artifact; arc42 ch.10).
#
#   editorconfig.editorconfig — a genuinely Node-only extension: package.json has a
#   `main` and NO `browser` entrypoint, so it CANNOT run in vscode-web's web ext
#   host; activating it proves the substrate-native (wasm-exec) Node-API host did
#   the work (CC-48, not CC-19). Its `activate()` exercises the real Node surface:
#   a bare `require('editorconfig')` (bundled node_modules) and a load-time
#   `fs.readFileSync` of @one-ini's bundled `.wasm`.
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NS=editorconfig NAME=editorconfig VER=0.18.2
OUT="$DIR/${NS}.${NAME}-${VER}.vsix"
URL="https://open-vsx.org/api/${NS}/${NAME}/${VER}/file/${NS}.${NAME}-${VER}.vsix"
curl -fsSL "$URL" -o "$OUT"
sha256sum "$OUT" | awk -v f="${NS}.${NAME}-${VER}.vsix" '{print $1"  "f}' > "$DIR/cc48.sha256"
echo "wrote $OUT + cc48.sha256"
