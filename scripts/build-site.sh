#!/usr/bin/env bash
#
# Build the GitHub Pages site locally into _site/.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FINAL_SITE_DIR="${1:-$ROOT/_site}"
MARKER="$FINAL_SITE_DIR/.holospaces-site-build"
SITE_TMP="$ROOT/target/site-tmp"
STAGE_DIR="$ROOT/target/site-build-$$"
BACKUP_DIR="$ROOT/target/site-prev-$$"

cleanup() {
    rm -rf "$STAGE_DIR" "$BACKUP_DIR"
}
trap cleanup EXIT

find_wasm_bindgen() {
    local version="$1"
    local candidate

    if command -v wasm-bindgen >/dev/null 2>&1; then
        candidate="$(command -v wasm-bindgen)"
        if [ "$("$candidate" --version)" = "wasm-bindgen $version" ]; then
            printf '%s\n' "$(dirname "$candidate")"
            return 0
        fi
    fi

    for candidate in "$HOME"/.cache/.wasm-pack/wasm-bindgen-*/wasm-bindgen "$ROOT"/target/.wasm-pack/wasm-bindgen-*/wasm-bindgen; do
        if [ -x "$candidate" ] && [ "$("$candidate" --version)" = "wasm-bindgen $version" ]; then
            printf '%s\n' "$(dirname "$candidate")"
            return 0
        fi
    done

    return 1
}

cd "$ROOT"

if [ -e "$FINAL_SITE_DIR" ] && [ ! -f "$MARKER" ]; then
    printf 'build-site: ERROR: %s exists but is not marked as a holospaces generated site; remove it or pass another output directory\n' "$FINAL_SITE_DIR" >&2
    exit 1
fi

rm -rf "$STAGE_DIR" "$BACKUP_DIR"
mkdir -p "$STAGE_DIR" "$SITE_TMP"
touch "$STAGE_DIR/.holospaces-site-build"

echo "build-site: generating browser fixtures"
cargo run -q -p holospaces --example holo_fixture -- crates/holospaces-web/web

echo "build-site: building Platform Manager wasm"
wasm_bindgen_version=$(awk '/name = "wasm-bindgen"/ { getline; gsub(/"/, "", $3); print $3; exit }' crates/holospaces-web/Cargo.lock)
wasm_pack_args=()
if wasm_bindgen_dir="$(find_wasm_bindgen "$wasm_bindgen_version")"; then
    export PATH="$wasm_bindgen_dir:$PATH"
    wasm_pack_args=(--mode no-install --no-opt)
fi
TMPDIR="$SITE_TMP" wasm-pack build "${wasm_pack_args[@]}" crates/holospaces-web --release --target web --out-dir web/pkg

echo "build-site: assembling static site"
cp crates/holospaces-web/web/index.html "$STAGE_DIR/"
cp crates/holospaces-web/web/workspace.html "$STAGE_DIR/"
cp crates/holospaces-web/web/fixture.holo "$STAGE_DIR/"
cp crates/holospaces-web/web/fixture-userland.wasm "$STAGE_DIR/"
cp -r crates/holospaces-web/web/pkg "$STAGE_DIR/pkg"
rm -f "$STAGE_DIR"/pkg/*.d.ts "$STAGE_DIR/pkg/package.json" "$STAGE_DIR/pkg/.gitignore"

cp vv/artifacts/cc11/Image.gz "$STAGE_DIR/workspace-kernel.gz"
cp vv/artifacts/cc9/linux/holospaces.dtb "$STAGE_DIR/workspace.dtb"
cp -r vv/artifacts/cc13/vendor "$STAGE_DIR/vendor"

cp vv/artifacts/cc14/kernel/Image.gz "$STAGE_DIR/devcontainer-kernel.gz"
mfdig=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['manifests'][0]['digest'].split(':')[1])" vv/artifacts/cc22/image/index.json)
ldig=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['layers'][0]['digest'].split(':')[1])" "vv/artifacts/cc22/image/blobs/sha256/$mfdig")
cp "vv/artifacts/cc22/image/blobs/sha256/$ldig" "$STAGE_DIR/devcontainer-layer.tar.gz"

cp vv/artifacts/cc16/kernel/Image.gz "$STAGE_DIR/devcontainer-net-kernel.gz"
nmfdig=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['manifests'][0]['digest'].split(':')[1])" vv/artifacts/cc16/image/index.json)
nldig=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['layers'][0]['digest'].split(':')[1])" "vv/artifacts/cc16/image/blobs/sha256/$nmfdig")
cp "vv/artifacts/cc16/image/blobs/sha256/$nldig" "$STAGE_DIR/devcontainer-net-layer.tar.gz"

echo "build-site: composing real VS Code workbench"
node crates/holospaces-web/web/build-workbench.mjs "$STAGE_DIR"

if [ -e "$FINAL_SITE_DIR" ]; then
    mv "$FINAL_SITE_DIR" "$BACKUP_DIR"
fi
mv "$STAGE_DIR" "$FINAL_SITE_DIR"
rm -rf "$BACKUP_DIR"

echo "build-site: wrote $FINAL_SITE_DIR"
