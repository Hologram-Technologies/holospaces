#!/usr/bin/env bash
#
# CC-14 — the devcontainer boots a real OS root filesystem over a VirtIO block
#         device (OASIS VirtIO v1.2: virtio-mmio + virtio-blk; RISC-V PLIC;
#         a real OCI base image as an ext4 κ-disk; ADR-011)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). Two witnesses, both against external authorities (vv/artifacts/cc14,
# provenance in SOURCE.txt; the differential oracle is qemu-system-riscv64):
#
#   1. the Layer Assembler turns a real OCI image's layers into an ext4 image
#      that e2fsprogs finds clean and reads back byte-identical
#      (crates/holospaces/tests/cc14_assembly.rs);
#   2. a real, unmodified Linux kernel boots on the emulator and MOUNTS that
#      assembled rootfs over the emulator's virtio-blk, running userspace —
#      behaviour matching QEMU (crates/holospaces/tests/cc14_virtio_block.rs,
#      a release-mode real-OS boot; ~17 s).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc14-virtio-block: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# 1) Layer Assembler vs e2fsprogs (fast).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc14_assembly -- --nocapture || exit 1

# 2) Real Linux mounts the assembled OCI rootfs over the emulator's virtio-blk
#    (release; the heavy real-OS boot, #[ignore] by default).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc14_virtio_block -- --ignored --nocapture \
    a_real_linux_mounts_an_assembled_oci_rootfs_over_virtio_blk || exit 1
