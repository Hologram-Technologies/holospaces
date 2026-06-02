#!/usr/bin/env bash
#
# CC-25 — the devcontainer's Dev Container Features install on create (ADR-016)
#
# A Codespace/Gitpod installs the features a devcontainer.json declares — each a
# published OCI artifact (devcontainer-feature.json + install.sh) whose script
# runs in the container during the build, before the lifecycle commands.
# holospaces realizes this: it parses `features` (CC-4), imports each feature
# artifact by κ (the CC-20 machinery), places it into the rootfs, and the
# generated /init runs each feature's install.sh before the lifecycle (CC-22),
# with the declared options as uppercased environment.
# Authority: the Dev Container Features spec (feature format + install contract)
# + the ext4 format (e2fsprogs); differential runtime oracle: qemu-system-riscv64.
# Witness: crates/holospaces/tests/cc25_features.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc25-features: SKIP — cargo unavailable" >&2; exit 127; fi

# (1) the feature is parsed + honoured: the /init installs it before the lifecycle,
# and the feature's files are in the assembled ext4 rootfs (e2fsprogs) — deterministic.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc25_features -- --nocapture || exit 1

# (2) holospaces' OWN emulator runs the feature's install.sh in the OS, before the
# lifecycle — FEATURE-INSTALLED:<option> appears (the option applied), no QEMU.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc25_features -- --ignored --nocapture \
    the_emulator_installs_the_feature || exit 1

# (3) the differential oracle — qemu-system-riscv64 produces the same markers.
if command -v qemu-system-riscv64 >/dev/null 2>&1; then
    cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
        --test cc25_features -- --ignored --nocapture qemu_installs_the_feature || exit 1
else
    echo "cc25-features: SKIP differential oracle — qemu-system-riscv64 unavailable" >&2
fi
