#!/usr/bin/env bash
#
# CC-61 — a real docker (OCI) image becomes ONE κ-addressable `.holo` via the
#         sparse-file (OPFS) streaming path: byte-identical, sound, reproducible
#
# Component conformance suite (arc42 ch.10). The foundation of "import any image →
# run 100% serverless in any browser": the deployed peer never holds a multi-GiB
# rootfs in the wasm heap — it assembles the ext4 block-by-block straight into a
# sparse OPFS file (only non-zero blocks written; holes stay sparse), then pages
# sectors on demand ("the KappaStore IS the memory, RAM is a cache", Laws L3/L4).
#
# Witness: crates/holospaces/tests/cc61_image_to_holo.rs ::
#   a_real_oci_image_streams_into_one_reproducible_holo — on the real BuildKit OCI
#   image (CC-10 fixture), assembling via stream_ext4_image_bootable into a sparse
#   std::fs::File (the OPFS pattern: seek + write + set_len) proves: (1) byte-
#   identical to the dense assemble_ext4_bootable (Law L1); (2) bounded peak memory
#   (materialized non-zero bytes ≪ the declared disk); (3) e2fsck -fn clean (the
#   e2fsprogs oracle, where present — skipped gracefully otherwise, the byte-identity
#   + κ proofs still hold); (4) one reproducible image_kappa (the `.holo` handle is
#   content-addressed, identical on any peer).
#
# Depends on: CC-10 (ingest), CC-14/50 (assembly + the streaming serializer).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc61-image-to-holo: SKIP — cargo not available in this environment" >&2
    exit 127
fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --test cc61_image_to_holo -- --nocapture || exit 1

echo "cc61-image-to-holo: PASS"
