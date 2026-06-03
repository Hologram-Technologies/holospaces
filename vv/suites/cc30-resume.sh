#!/usr/bin/env bash
#
# CC-30 — a suspended machine resumes from its κ snapshot (ADR-009)
#
# Emulator::snapshot captures a running machine as canonical, content-addressed
# bytes; Emulator::restore is its inverse, so suspend → resume is a round trip.
# This is the foundation for a second launch that resumes instead of cold-booting.
#
# Authority/oracle: the snapshot's own determinism (Law L1) and the
# qemu-system-riscv64 differential oracle — a real Linux machine suspended
# mid-boot and resumed from its κ reaches the byte-identical userspace the
# un-suspended machine did.
#
# Witness: crates/holospaces/tests/cc30_resume.rs —
#   * restore(snapshot(m)) re-snapshots to the same bytes/κ (faithful inverse),
#     including a machine with a virtio-blk disk;
#   * the resumed machine continues byte-identically (same console, same κ);
#   * resume is κ-addressed content (migrates by its κ); truncated input rejected;
#   * a real Linux boot suspended mid-flight resumes to the identical userspace.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc30-resume: SKIP — cargo unavailable" >&2; exit 127; fi
# Fast witnesses (round-trip identity, virtio, migration, truncation).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc30_resume -- --nocapture || exit 1
# The real-Linux differential resume (release; boots Linux twice).
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc30_resume a_suspended_real_linux_machine_resumes_to_the_identical_boot \
    -- --ignored --nocapture || exit 1
# CC-31 resume terminal: the deployed devcontainer, suspended at its *idle shell*
# (the steady state the periodic snapshot actually captures — not mid-boot),
# resumes to a LIVE machine, and the machine snapshot is κ-pure (the console
# scrollback is a terminal concern, not machine state). Release; boots the
# devcontainer to userspace.
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc31_resume_terminal -- --ignored --nocapture || exit 1
