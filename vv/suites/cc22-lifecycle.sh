#!/usr/bin/env bash
#
# CC-22 — the devcontainer's lifecycle commands run on create (ADR-016)
#
# A Codespace/Gitpod runs the Dev Container lifecycle commands (postCreateCommand
# etc.) so the environment is ready on entry. holospaces realizes this: the Boot
# Orchestrator parses the commands from devcontainer.json (CC-4), builds an /init
# lifecycle runner from the parsed config, and injects it into the assembled
# rootfs over a base image that provides a shell — so the booted OS runs the
# declared commands in spec order, with the config's remoteEnv applied.
# Authority: the Dev Container spec (hooks + run order) + the ext4 format
# (e2fsprogs); differential runtime oracle: qemu-system-riscv64.
# Witness: crates/holospaces/tests/cc22_lifecycle.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc22-lifecycle: SKIP — cargo unavailable" >&2; exit 127; fi

# (1) build-from-config + (2) ext4 injection (e2fsprogs oracle) — deterministic.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc22_lifecycle -- --nocapture || exit 1

# (3) the real OS runs the lifecycle commands — qemu-system-riscv64 boots the
# holospaces-assembled rootfs and the declared postCreateCommand output appears.
if command -v qemu-system-riscv64 >/dev/null 2>&1; then
    cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
        --test cc22_lifecycle -- --ignored --nocapture \
        the_os_runs_the_devcontainer_lifecycle_commands || exit 1
else
    echo "cc22-lifecycle: SKIP runtime oracle — qemu-system-riscv64 unavailable" >&2
fi
