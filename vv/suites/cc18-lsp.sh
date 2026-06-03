#!/usr/bin/env bash
#
# CC-18 — the workbench provides language intelligence (LSP) (ADR-012/015)
#
# The workbench provides language intelligence by speaking the Language Server
# Protocol to a real language server running in the devcontainer OS. holospaces
# runs a real LSP server (lsp-demo, built on lsp-types — rust-analyzer's own LSP
# type crate) inside the booted OS; the workbench's session flows to it over the
# standard LSP stdio transport and its responses conform to the LSP spec.
# Authority: the LSP spec (via lsp-types) + a real language server in the OS;
# ext4 (e2fsprogs) confirms the binary + session are in the rootfs; holospaces'
# own emulator runs the server under a real libc.
# Witness: crates/holospaces/tests/cc18_lsp.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc18-lsp: SKIP — cargo unavailable" >&2; exit 127; fi

# (1) the LSP session is spec-valid + the language server binary + session are in
# the assembled ext4 rootfs (e2fsprogs oracle) — deterministic.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc18_lsp -- --nocapture || exit 1

# (2) holospaces' OWN emulator runs the real language server in the OS and it
# speaks LSP — the responses conform to the spec (validated via lsp-types).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc18_lsp -- --ignored --nocapture \
    the_in_os_language_server_speaks_lsp || exit 1

# (3) the DEPLOYED delivery (ADR-020): the SAME language server runs as a TCP
# service in the OS (lsp-demo --listen) and the workbench drives a full LSP
# session to it over the in-process substrate bridge (CC-33) — every response
# (capabilities, hover, completion, definition, diagnostics) conforms to the LSP
# spec via lsp-types. Real language intelligence in the browser tab, no Node.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc18_lsp_bridge -- --ignored --nocapture \
    the_in_os_language_server_serves_the_workbench_over_the_bridge || exit 1
