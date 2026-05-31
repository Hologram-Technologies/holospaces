//! Run a single official RISC-V `riscv-tests` conformance binary on the emulator
//! core (CC-9 bring-up / debugging). Loads a flat `-p` test image at 0x80000000,
//! sets its HTIF `tohost` address, and reports the exit code (0 = pass) plus the
//! failing test number (`gp`). With a third argument it traces every trap.
//!
//! `cargo run --release --example rvrun -- <test.bin> <tohost-hex> [trace]`

fn main() {
    let mut a = std::env::args().skip(1);
    let bin = a.next().expect("usage: rvrun <test.bin> <tohost> [trace]");
    let tohost = u64::from_str_radix(a.next().expect("tohost").trim_start_matches("0x"), 16)
        .expect("tohost is hex");
    let trace = a.next().is_some();

    let img = std::fs::read(&bin).expect("read the test image");
    let mut emu = holospaces::emulator::Emulator::new(0x8000_0000, 16 * 1024 * 1024);
    emu.load_flat(&img).expect("image fits in RAM");
    emu.set_htif(tohost);

    if trace {
        let mut prev_cause = u64::MAX;
        for i in 0..2_000_000u64 {
            let pc = emu.pc();
            let cause = emu.csr(0x342);
            if cause != prev_cause {
                eprintln!(
                    "[{i:7}] trap mcause={cause:#x} mepc={:#x} mtval={:#x} (pc={pc:#x}) gp={}",
                    emu.csr(0x341),
                    emu.csr(0x343),
                    emu.xreg(3)
                );
                prev_cause = cause;
            }
            if let Err(h) = emu.step_once() {
                eprintln!("HALT {h:?} at pc={pc:#x} gp={}", emu.xreg(3));
                return;
            }
        }
        eprintln!(
            "(trace budget exhausted) pc={:#x} gp={}",
            emu.pc(),
            emu.xreg(3)
        );
        return;
    }

    let halt = emu.run(50_000_000);
    println!("{halt:?} gp={} pc={:#x}", emu.xreg(3), emu.pc());
    // The HTIF result word (the test writes (num<<1)|1) — a content peek.
    let _ = emu.peek(tohost, 8);
}
