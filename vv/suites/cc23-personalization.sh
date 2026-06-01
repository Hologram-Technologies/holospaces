#!/usr/bin/env bash
#
# CC-23 — the workspace is personalized: settings, dotfiles, and secrets (ADR-016)
#
# A Codespace/Gitpod carries an operator's personalization so their environment
# is ready wherever they sign in. holospaces realizes this WITHOUT a server
# account: a Personalization is κ-addressed content that embeds the operator
# identity (CC-1/CC-12), held in the store and synced by the substrate (Laws
# L1/L3); on entry holospaces applies it — the dotfiles are injected into the
# devcontainer OS's home directory and the secrets are exported into its
# environment by an entry /init, the editor settings handed to the workbench.
# Authority: the Dev Container spec (remoteEnv/secrets as environment), the
# Codespaces/Gitpod dotfiles convention, and the ext4 format (e2fsprogs);
# the booted OS applying it under a real libc (busybox) shell is witnessed on
# holospaces' own emulator. Witness: crates/holospaces/tests/cc23_personalization.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc23-personalization: SKIP — cargo unavailable" >&2; exit 127; fi

# unit: the personalization is content scoped to the operator (reproducible κ;
# different operator → different κ), round-trips, and applies secrets + dotfiles.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --lib personalization -- --nocapture || exit 1

# (1) the operator's dotfiles + entry runner are injected into the assembled ext4
# rootfs — e2fsck clean + debugfs reads them back byte-identically (deterministic).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc23_personalization -- --nocapture || exit 1

# (2) holospaces' OWN emulator applies the personalization under a real libc
# (busybox) shell — the secret is present in the OS environment (not leaked) and
# the operator's dotfile is in the home directory. The substrate the holospace
# actually boots on, no QEMU.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc23_personalization -- --ignored --nocapture \
    the_holospaces_emulator_applies_the_personalization || exit 1
