#!/usr/bin/env bash
#
# CC-59 — "resume, don't re-run": warm κ-resume of the amd64 (x86-64) machine is
#         instant, faithful, and far faster than a cold boot
#
# Component conformance suite (arc42 ch.10). amd64's remaining serve blocker is
# wall-clock: the switch-root kernel boots in ~30 min on the pure-Rust x86-64
# interpreter (vs ~6 s riscv), and a region JIT is a MEASURED net loss on real
# short-region workloads (cc45 region_jit_real_alpine_speedup). The speed answer
# is to resume the state the planet computed once, not to re-run the boot:
# snapshot a running machine as a content-addressed κ (Cpu::snapshot_kappa) and
# restore it into a fresh core (Cpu::restore_kappa) — page-reconstruct + verify
# (L5), not a re-boot.
#
# Two witnesses, the machine itself as the Law-L1 oracle (KappaSnapshot::
# to_manifest_bytes is the deterministic content label of the WHOLE machine):
#
#   1. CI-cheap fixed-point (default gate, NO boot) — crates/holospaces/tests/
#      cc59_resume_fixed_point.rs::warm_resume_is_instant_and_a_fixed_point:
#      snapshots a short mid-boot window, resumes with ZERO guest instructions,
#      and proves the resumed machine is byte-identical to the never-snapshotted
#      one. Guards snapshot/restore fidelity without a multi-minute boot.
#   The bit-exact-to-userspace gates (kappa_snapshot_midboot_restore_is_bit_exact_
#   to_userspace, kappa_snapshot_kappa_resume_to_userspace) run in the default unit
#   gate. The SPEED HEADLINE (cold boot vs κ-resume wall-clock) is a heavy on-demand
#   witness — vv/heavy/cc59-warm-resume-speed.sh — kept off this per-push gate
#   because the cold boot it times is ~tens of seconds (release) / minutes (debug).
#
# Depends on: CC-44 (the amd64 boot + the snapshot_kappa/restore_kappa machinery).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc59-amd64-warm-resume: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# CI-cheap fixed-point guard (no boot; the standing regression witness).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc59_resume_fixed_point -- --nocapture || exit 1

echo "cc59-amd64-warm-resume: PASS"
