#!/usr/bin/env bash
#
# CC-9 — the system emulator boots a real operating system (ADR-009)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). The emulator is the implementation under test; the authorities are:
#   • the official RISC-V riscv-tests conformance suite (the same battery real
#     hardware and QEMU are validated against; vv/artifacts/cc9/riscv-tests/);
#   • a real, unmodified Linux 6.6 kernel that must boot to userspace on the
#     emulator and produce output byte-identical to qemu-system-riscv64 on the
#     same image (vv/artifacts/cc9/linux/, the differential oracle);
#   • the real hologram Wasmtime runtime (the emulator runs as a κ-addressed
#     container codemodule).
# Witness: crates/holospaces/tests/cc9_emulator.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc9-emulator: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# The fast cargo-tier witnesses: ISA conformance (134 official riscv-tests),
# the CLINT timer, SBI, the codemodule on the real runtime, and the κ-disk.
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc9_emulator -- --nocapture || exit 1

# The OS-boot witness: a real Linux kernel boots to userspace. A full boot is
# ~15 s and must run optimised, so it is #[ignore]d in the cargo tier and run
# here in release.
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc9_emulator the_emulator_boots_real_linux_to_userspace \
    -- --ignored --nocapture || exit 1

# The substrate witness: the same Linux kernel boots *through* the emulator
# codemodule on the real hologram runtime (image delivered as κ content via
# storage_get), emitting a record byte-identical to the core. Boots Linux twice
# (~90 s); the flagship CC-9 witness — ADR-009 fully realized.
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc9_emulator the_codemodule_boots_real_linux_on_the_substrate \
    -- --ignored --nocapture || exit 1

# The differential oracle, when the reference implementation is available: the
# same pinned Image must boot identically on qemu-system-riscv64. The emulator
# witness already asserts the userspace output equals expected-userspace.txt
# (captured from QEMU); this step re-derives that capture live when QEMU is
# present, so the oracle is never stale.
LINUX="$ROOT/vv/artifacts/cc9/linux"
if command -v qemu-system-riscv64 >/dev/null 2>&1; then
    tmp="$(mktemp -d)"
    gzip -dc "$LINUX/Image.gz" > "$tmp/Image"
    timeout 90 qemu-system-riscv64 -M virt -m 128M -nographic -bios default \
        -kernel "$tmp/Image" -append "console=hvc0 earlycon=sbi" 2>&1 \
        | tr -d '\r' > "$tmp/qemu.log"
    if grep -aq 'HOLOSPACES-LINUX-USERSPACE-OK' "$tmp/qemu.log" \
       && diff <(grep -aA1 'USERSPACE-OK' "$tmp/qemu.log") "$LINUX/expected-userspace.txt" >/dev/null; then
        echo "cc9-emulator: qemu-system-riscv64 differential PASS (oracle current)"
    else
        echo "cc9-emulator: qemu-system-riscv64 differential FAILED — oracle drift" >&2
        rm -rf "$tmp"; exit 1
    fi
    rm -rf "$tmp"
else
    echo "cc9-emulator: qemu-system-riscv64 absent — differential pinned in expected-userspace.txt (captured from QEMU, per linux/SOURCE.txt)"
fi
