#!/usr/bin/env bash
#
# CC-58 — the assembled ext4 preserves file MODE BITS (the executable bit on
#         `/init` and on image binaries)
#
# Component conformance suite (arc42 chapter 10). A real image whose `/init` is a
# shell script (`#!/bin/busybox sh`) or whose binaries need `+x` only boots if the
# assembled ext4 carries each entry's permission bits — a kernel `exec` of a
# non-executable `/init` fails `EACCES (-13)`. The mode must survive the whole
# pipeline: tar USTAR mode (header offset 100, octal) → the overlay `Node`'s
# `Meta.mode` → the ext4 writer's inode `i_mode`, and the injected-file path
# (`assemble_ext4_with_files`) must honour its `u16` mode argument.
#
# Witness (against the same external oracle as CC-14, e2fsprogs — NOT a
# self-referential read of holospaces' own writer):
#   crates/holospaces/tests/cc58_ext4_perms.rs ::
#   `assembled_ext4_preserves_file_mode_bits` — assembles a rootfs with an
#   executable (0755) `/init`, an executable (0755) tar-sourced binary, and
#   non-executable (0644) files; `e2fsck -fn` must be clean and `debugfs -R
#   "stat <path>"` must report Mode 0755 / 0644 for each (the on-disk inode
#   decoded independently of the writer). Fast (no boot): the CI-cheap behavioural
#   proof that `exec /init` will not fail `EACCES`.
#
# The witness skips gracefully where e2fsprogs is absent (mirroring CC-14); on a
# host with e2fsprogs it is the gating oracle. Validated green under e2fsprogs
# 1.47.2 (provenance: vv/PROVENANCE.md).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc58-ext4-perms: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# Layer Assembler mode-preservation vs e2fsprogs (fast; no boot).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc58_ext4_perms -- --nocapture || exit 1
