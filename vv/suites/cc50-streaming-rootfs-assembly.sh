#!/usr/bin/env bash
#
# CC-50 — Provisioning assembles an arbitrarily large rootfs without a dense
#         in-memory image (sparse, streaming assembly)
#
# Component conformance suite (arc42 ch.10). OPM process: SD5 Rootfs Assembly
# ("the KappaStore IS the memory, RAM is a cache"). A holospace's disk is
# content-addressed sectors paged from a KappaStore (CC-42); this makes the
# assembly that *produces* those sectors stream straight into the store rather
# than materializing one dense Vec. The ext4 serializer emits only the non-zero
# 4 KiB blocks (holes and the free data region stay sparse), so peak working
# memory tracks the image's content, not its declared size — a multi-GiB disk
# whose free space is sparse provisions and boots without a multi-GiB allocation.
#
# Authority: the ext4 on-disk format (the assembled rootfs is a valid, bootable
#   filesystem — e2fsck is the external oracle) and the substrate's
#   content-addressed store as the medium; differential = the streamed assembly is
#   κ-IDENTICAL to the dense assembly (same image_kappa, byte-identical sectors).
# Witness: crates/holospaces/tests/cc50_streaming_assembly.rs — a rootfs much
#   larger than its content assembles by streaming into a KappaStore with peak
#   materialized bytes ≪ image size; the streamed κ-disk's image_kappa equals the
#   dense path's and reads back byte-identical; e2fsck finds the streamed image
#   structurally clean.
#
# The single canonical ext4 serializer (assembly/ext4.rs) is shared by the dense
# (write_image_with_free) and streaming (stream_image_with_free / sparse_blocks_
# with_free) consumers, so they are byte-identical by construction; KappaDisk
# treats a written all-zero sector as sparse, so dense from_image and streaming
# from_block_stream yield the same sector κ-set for the same content (Law L1).
#
# The *deployed browser* path is witnessed by the browser job (the CI browser job
# / scripts/browser-manager-test.sh → crates/holospaces-web/web/cc50-streaming-
# boot-test.mjs): in Chromium, a dedicated worker streams the bootable rootfs
# straight into an OPFS file (the shared stream_ext4_image_bootable serializer the
# wasm assembleIntoOpfs uses) and BOOTS it to a userspace marker via the shipped
# Workspace.boot_devcontainer_routed_opfs_streamed paged-κ-disk path — so the
# streamed-into-OPFS image is proven to BOOT for real, not only κ-identical.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc50-streaming-rootfs-assembly: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc50_streaming_assembly -- --nocapture \
    || exit 1

echo "cc50-streaming-rootfs-assembly: PASS"
