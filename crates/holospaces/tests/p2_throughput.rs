//! P2 — emulator throughput probe (informational; `#[ignore]`).
//!
//! Not a conformance witness — correctness is proven byte-for-byte by the CC-9 /
//! CC-14 differential oracle. This boots the same pinned RISC-V Linux kernel to
//! userspace and reports wall-clock time and instructions/sec, so the throughput
//! win from the P2 stages (RAM fast path, bulk memory, the software TLB) is a
//! recorded number rather than a claim. Run it with:
//!
//! ```text
//! cargo test --release -p holospaces --test p2_throughput -- --ignored --nocapture
//! ```
//!
//! (Release only — a debug build runs the interpreter ~10× slower and is not a
//! meaningful throughput figure.) MIPS is machine-dependent; the recorded number
//! is for tracking, and the only gate this test asserts is a catastrophe floor.

use holospaces::emulator::{Emulator, Halt};
use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

/// `INSTRET` (0xC02) mirrors the per-instruction tick counter — the retired
/// count to within the (rare) taken-interrupt redirects.
const INSTRET: u32 = 0xc02;

fn cc9_linux_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vv/artifacts/cc9/linux")
        .canonicalize()
        .expect("the CC-9 Linux artifact directory")
}

/// Informational probe (`#[ignore]`d): boots the pinned RISC-V Linux kernel to a
/// clean userspace power-off and **prints** the throughput figures (wall-clock,
/// instruction count, MIPS — the proxy for the TLB-on/off speedup). The only
/// assertion is a *catastrophe floor* (`mips > 1.0`) guarding against a gross
/// regression; the printed MIPS / speedup ratio is recorded, not asserted.
#[test]
#[ignore = "informational throughput probe; run explicitly in --release"]
fn emulator_boots_real_linux_throughput() {
    let dir = cc9_linux_dir();
    let gz = std::fs::read(dir.join("Image.gz")).expect("Image.gz (see linux/SOURCE.txt)");
    let mut image = Vec::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_end(&mut image)
        .expect("gunzip the kernel Image");
    let dtb = std::fs::read(dir.join("holospaces.dtb")).expect("holospaces.dtb");

    let base = 0x8000_0000u64;
    let mut emu = Emulator::new(base, 128 * 1024 * 1024);
    emu.boot_kernel(&image, &dtb, base + 0x0700_0000)
        .expect("load the kernel Image + device tree");

    // Time only the emulation (the kernel decompress + DTB load above is setup).
    let start = Instant::now();
    let halt = loop {
        match emu.run(10_000_000) {
            Halt::OutOfBudget => {
                assert!(
                    emu.csr(INSTRET) < 5_000_000_000,
                    "the kernel did not reach userspace within the budget"
                );
            }
            other => break other,
        }
    };
    let elapsed = start.elapsed();
    assert_eq!(halt, Halt::Exit(0), "PID 1 powers the machine off cleanly");

    let instrs = emu.csr(INSTRET);
    let secs = elapsed.as_secs_f64();
    let mips = (instrs as f64) / secs / 1.0e6;
    println!(
        "P2 throughput: booted real RISC-V Linux to userspace exit in {secs:.2}s — \
         {instrs} instructions, {mips:.1} MIPS"
    );
    // Catastrophe floor — not a tight gate (CI-runner speed varies several-fold),
    // but a guard against a *gross* regression (e.g. the TLB or the RAM fast path
    // silently disabled, which would drop throughput by an order of magnitude).
    // The recorded MIPS above is the real signal for tracking; this only trips on
    // disaster, so it never flakes.
    assert!(
        mips > 1.0,
        "emulator throughput collapsed to {mips:.1} MIPS — a catastrophic (>10×) \
         regression; the optimization path is likely disabled"
    );
}
