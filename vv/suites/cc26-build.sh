#!/usr/bin/env bash
#
# CC-26 — a Dockerfile-build devcontainer is built on the substrate (ADR-011/016)
#
# holospaces parses the Dockerfile, pulls FROM (CC-20), injects the COPY sources,
# and runs the RUN instructions in the devcontainer OS (CC-22/CC-25 machinery) —
# the build, no Docker daemon. Authority: the Dockerfile reference + the Dev
# Container `build`; ext4 (e2fsprogs); differential oracle qemu-system-riscv64.
# Witness: crates/holospaces/tests/cc26_build.rs + the dockerfile unit tests.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc26-build: SKIP — cargo unavailable" >&2; exit 127; fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release --lib dockerfile -- --nocapture || exit 1
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release --test cc26_build -- --nocapture || exit 1
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc26_build -- --ignored --nocapture the_emulator_runs_the_build || exit 1
if command -v qemu-system-riscv64 >/dev/null 2>&1; then
    cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
        --test cc26_build -- --ignored --nocapture qemu_runs_the_build || exit 1
else
    echo "cc26-build: SKIP differential oracle — qemu-system-riscv64 unavailable" >&2
fi
