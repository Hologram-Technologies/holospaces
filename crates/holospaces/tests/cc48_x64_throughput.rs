//! CC-48 — x86-64 guest-throughput probe (informational; `#[ignore]`).
//!
//! Not a conformance witness — correctness is the CC-44 differential oracle. This
//! boots the same pinned amd64 Linux kernel the CC-44 witness uses and reports
//! wall-clock time and **guest MIPS** (retired guest instructions / sec), so the
//! interpreter's throughput is a *recorded, reproducible number* rather than a
//! claim. It is the measured baseline the substrate fast-execution path (the
//! x86-64 → wasm DBT) must clear for CC-48: the in-guest `openvscode-server`
//! (Node/V8) needs far more headroom than a plain interpreter gives.
//!
//! ```text
//! cargo test --release -p holospaces --test cc48_x64_throughput -- --ignored --nocapture
//! ```
//!
//! Release only — a debug build runs the interpreter ~10× slower and is not a
//! meaningful figure. MIPS is machine-dependent; the portable signal is the
//! *ratio* between this interpreter baseline and a fast-path build.

use std::io::Read;
use std::path::Path;
use std::time::Instant;

use holospaces::emulator::x64::{Cpu, Halt};

fn vmlinux_elf() -> Vec<u8> {
    let gz = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc44/linux/vmlinux.gz");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(&gz).expect("read cc44 vmlinux.gz")[..])
        .read_to_end(&mut img)
        .expect("gunzip the kernel ELF");
    img
}

#[test]
#[ignore = "informational throughput probe; run explicitly in --release"]
fn x64_guest_throughput_booting_real_linux() {
    let kernel = vmlinux_elf();
    let mut cpu = Cpu::boot_linux(
        1024 * 1024 * 1024,
        &kernel,
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
    );

    // Time only the emulation (the ELF load + zero-page build in boot_linux is
    // setup). Run in bounded slices so the probe can't spin forever if the boot
    // regresses, but the workload is the same byte-identical boot the CC-44
    // witness runs to a clean power-off.
    let start = Instant::now();
    let halt = loop {
        match cpu.run(2_000_000_000) {
            Halt::OutOfBudget => {
                assert!(
                    cpu.insns() < 40_000_000_000,
                    "the kernel did not reach userspace + power-off within the budget"
                );
            }
            other => break other,
        }
    };
    let elapsed = start.elapsed();
    assert_eq!(halt, Halt::Halted, "PID 1 powers the machine off cleanly");

    let instrs = cpu.insns();
    let secs = elapsed.as_secs_f64();
    let mips = (instrs as f64) / secs / 1.0e6;
    println!(
        "CC-48 baseline: booted real amd64 Linux to userspace power-off in {secs:.2}s — \
         {instrs} guest instructions, {mips:.1} guest MIPS (interpreter, no fast path)"
    );

    // Catastrophe floor only (CI-runner speed varies several-fold): a meaningful
    // boot retires many millions of instructions. The printed MIPS is the signal.
    assert!(
        instrs > 100_000_000,
        "a real boot retires >100M guest instructions (got {instrs})"
    );
}
