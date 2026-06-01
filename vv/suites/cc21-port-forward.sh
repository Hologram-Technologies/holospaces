#!/usr/bin/env bash
#
# CC-21 — a server in the devcontainer is reachable as a forwarded port (ADR-016)
#
# The running-app preview a Codespace/Gitpod surfaces: holospaces forwards a host
# port to a server inside the devcontainer via the INGRESS dual of the CC-16
# userspace NAT — it accepts an inbound connection and opens a connection TO the
# guest's listening port (the NAT is the active opener toward the guest). A host
# client reaches the guest server through the forward and reads its response.
# Authority: the VirtIO spec + TCP/IP; oracle: qemu-system-riscv64's hostfwd.
# Witness: crates/holospaces/tests/cc21_port_forward.rs (release; a real-OS boot).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc21-port-forward: SKIP — cargo unavailable" >&2; exit 127; fi
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc21_port_forward -- --ignored --nocapture \
    a_server_in_the_devcontainer_is_reachable_through_a_forwarded_port || exit 1
