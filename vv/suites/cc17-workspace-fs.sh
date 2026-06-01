#!/usr/bin/env bash
#
# CC-17 (Phase 2 foundation) — the editor-side filesystem over the shared
# workspace (ADR-012/015)
#
# The real VS Code web workbench (Phase 1) edits the holospace's files through a
# FileSystemProvider. Per ADR-012/015 that provider is NOT a separate store — it
# is the virtio-9p workspace of CC-15: the κ-addressed content the editor and the
# running OS share (Law L1). This suite exercises holospaces' editor-side API
# over that share (list / write / read), asserting content addressing and that a
# file the editor writes is the same content the guest OS reads over virtio-9p.
# The browser wiring (a service worker bridging the workbench's web-extension
# provider to the wasm peer) sits on top of this substrate API.
# Witness: crates/holospaces/tests/cc17_workspace_fs.rs (a fast unit witness +
# an #[ignore] real-OS boot witness; the latter reuses the CC-15 init).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc17-workspace-fs: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# The fast editor-FS witness (content addressing, list/write/read round-trip).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc17_workspace_fs the_editor_lists_writes_and_reads_the_shared_workspace_by_kappa || exit 1

# The real-OS boot witness: a file the editor writes is read by the OS over 9p.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc17_workspace_fs -- --ignored --nocapture \
    a_file_the_editor_writes_is_read_by_the_os_over_9p || exit 1
