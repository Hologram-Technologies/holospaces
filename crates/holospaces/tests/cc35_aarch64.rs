//! `CC-35` — the system emulator executes the AArch64 (A64) integer ISA
//! correctly (ADR-021, arc42 ch.10 conformance catalog).
//!
//! The implementation under test is the [`aarch64`](holospaces::emulator::aarch64)
//! integer core. The authority is the **Arm Architecture Reference Manual** (ARM
//! DDI 0487) for the A64 base instruction set + `PSTATE.NZCV`, with
//! `qemu-system-aarch64`/`qemu-aarch64` as the differential oracle. These
//! witnesses run **real, toolchain-assembled** A64 binaries
//! (`vv/artifacts/cc35/*.bin`, built from the committed `.s` sources by
//! `vv/artifacts/cc35/build.sh`): each is a self-checking battery that, run on
//! the core at its reset PC, writes `PASS\n` and exits `0` exactly when every
//! Arm-ARM-defined result holds — the same stdout + status `qemu-aarch64`
//! produces for the same machine code (`vv/suites/cc35-aarch64-core.sh`).

use holospaces::emulator::aarch64::{Cpu, Halt};

/// Load a committed A64 battery, run it on the core, and return its
/// `(console, exit_status)`. The battery is position-independent, so any reset
/// base works; 16 MiB of RAM gives the stack frame headroom.
fn run_battery(image: &[u8]) -> (Vec<u8>, u64) {
    const BASE: u64 = 0x4000_0000;
    let mut cpu = Cpu::new(BASE, 16 * 1024 * 1024);
    cpu.load_image(image);
    match cpu.run(10_000_000) {
        Halt::Exit(status) => (cpu.console().to_vec(), status),
        other => panic!("battery did not exit cleanly: {other:?}"),
    }
}

const ARITH: &[u8] = include_bytes!("../../../vv/artifacts/cc35/arith.bin");
const MEMORY: &[u8] = include_bytes!("../../../vv/artifacts/cc35/memory.bin");
const CONTROL: &[u8] = include_bytes!("../../../vv/artifacts/cc35/control.bin");

/// The data-processing battery: every A64 data-processing group's result equals
/// the Arm-ARM-defined value.
#[test]
fn the_a64_data_processing_battery_passes() {
    let (console, status) = run_battery(ARITH);
    assert_eq!(console, b"PASS\n", "arith battery verdict");
    assert_eq!(status, 0, "arith battery exit status");
}

/// The load/store battery: the full addressing-mode + extension family round
/// trips through memory correctly.
#[test]
fn the_a64_load_store_battery_passes() {
    let (console, status) = run_battery(MEMORY);
    assert_eq!(console, b"PASS\n", "memory battery verdict");
    assert_eq!(status, 0, "memory battery exit status");
}

/// The control-flow battery: branches + `NZCV` condition codes drive real loops,
/// a subroutine call, and the bit-test branches to the Arm-ARM-defined result
/// (`sum(1..=100) == 5050`).
#[test]
fn the_a64_control_flow_battery_passes() {
    let (console, status) = run_battery(CONTROL);
    assert_eq!(console, b"PASS\n", "control battery verdict");
    assert_eq!(status, 0, "control battery exit status");
}
