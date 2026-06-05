#!/usr/bin/env bash
#
# CC-50 (TARGET) — Provisioning assembles an arbitrarily large rootfs without
#                  materializing a dense in-memory image
#
# OPM process: SD5 Rootfs Assembly ("Rootfs Assembly requires Base Image → Root
# Filesystem"). The substrate principle is "the KappaStore IS the memory, RAM is
# a cache": a holospace's disk is content-addressed sectors paged from a
# KappaStore (the CC-42 paged κ-disk over OPFS). But the browser Manager's
# DevcontainerProvision.assemble still materializes the rootfs into one dense Vec
# (bounded to 1 GiB to avoid OOMing the tab). This target makes assembly stream
# sector-by-sector straight into the KappaStore — sparse, deduped, never a dense
# image — so an image larger than the tab's heap provisions and boots.
#
# Authority: the ext4 on-disk format (the assembled rootfs must be a valid,
#   bootable filesystem) and the substrate's content-addressed store as the
#   medium; differential = the streamed assembly equals the dense assembly κ-wise.
# Witness: crates/holospaces/tests/cc50_streaming_assembly.rs — a >heap rootfs
#   assembles streaming into a KappaStore (peak RAM ≪ image size) and yields the
#   IDENTICAL κ-set as the dense assembly, and boots.
#
# GREEN when: provisioning assembles a rootfs larger than a fixed RAM budget by
#   streaming sparse sectors into the KappaStore, peak memory bounded and the
#   result κ-identical to the dense path.
#
# Status: TARGET — not yet live. Expected RED (non-gating).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WITNESS="$ROOT/crates/holospaces/tests/cc50_streaming_assembly.rs"

if [ -f "$WITNESS" ]; then
    command -v cargo >/dev/null 2>&1 || { echo "cc50-streaming-rootfs-assembly: SKIP — cargo absent"; exit 127; }
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc50_streaming_assembly -- --nocapture || exit 1
    exit 0
fi

echo "cc50-streaming-rootfs-assembly: RED — TARGET not yet live."
echo "  observed: DevcontainerProvision.assemble materializes a dense Vec (bounded to 1 GiB)."
echo "  needed:   stream sparse sectors straight into the KappaStore; witness"
echo "            cc50_streaming_assembly.rs."
echo "  spec:     a >RAM-budget rootfs assembles with bounded peak memory, κ-identical to"
echo "            the dense path, and boots (the KappaStore IS the memory)."
exit 1
