#!/usr/bin/env bash
# Build the system-emulator codemodule (the RISC-V emulator core compiled to a
# hologram Wasm container) and stage it as the CC-9 artifact. The codemodule
# imports only `hologram.storage_put` and exports the container ABI (hg_*).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT/crates/holospaces-emulator"
cargo build --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/holospaces_emulator.wasm \
   "$ROOT/vv/artifacts/cc9/emulator.wasm"
echo "staged vv/artifacts/cc9/emulator.wasm"
