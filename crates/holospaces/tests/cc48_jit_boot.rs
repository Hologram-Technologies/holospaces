//! **CC-48 JIT boot + SMC gate** — the real-correctness witness for the
//! JIT-accelerated x86-64 execution path
//! ([`holospaces::emulator::x64_jit_exec::X64JitExec`] +
//! [`holospaces::emulator::x64::Cpu::run_jit`]).
//!
//! The pure-interpreter boot is the qemu-validated authority (CC-44). This test
//! drives the *same* unmodified amd64 Linux kernel through the JIT driver instead
//! of [`Cpu::run`] and asserts it reaches userspace **byte-correctly** — which
//! exercises the two hard paths the JIT must get right: massive demand-paging
//! (every block runs over the same `jit_mem`/`#PF` machinery the interpreter uses)
//! and self-modifying code (the kernel patches its own `.text` via alternatives /
//! `text_poke`, so a cached translated block whose source bytes change must be
//! invalidated and re-translated). It reports the measured speedup over the
//! interpreter baseline and the fraction of instructions executed through JIT
//! blocks vs the interpreter fallback.
//!
//! A focused SMC unit test ([`smc_invalidates_a_cached_block`]) and a driver smoke
//! test ([`jit_runs_a_register_block`]) run without the boot artifact (not
//! `#[ignore]`); the full boot is `#[ignore]` (release-only, heavy).

use std::io::Read;
use std::path::Path;
use std::time::Instant;

use holospaces::emulator::x64::{Cpu, Halt};
use holospaces::emulator::x64_jit_exec::X64JitExec;

// ── Driver smoke test: a register-only block runs through the JIT ──────────────

#[test]
fn jit_runs_a_register_block() {
    // A flat-RAM program (paging off): set rax/rcx, add, then hlt. The arithmetic
    // is a translatable block; `hlt` ends it (interpreter fallback) → Halted.
    //   mov eax, 5       B8 05 00 00 00
    //   mov ecx, 7       B9 07 00 00 00
    //   add eax, ecx     01 C8
    //   hlt              F4
    #[rustfmt::skip]
    let prog: &[u8] = &[
        0xB8, 0x05, 0x00, 0x00, 0x00,
        0xB9, 0x07, 0x00, 0x00, 0x00,
        0x01, 0xC8,
        0xF4,
    ];
    let mut cpu = Cpu::new(64 * 1024);
    cpu.load_at(0, prog);
    // Threshold 1 → compile each block on first sight, so the JIT path is exercised
    // deterministically (the default tiering threshold would interpret a block this
    // short).
    let mut exec = X64JitExec::new().with_hot_threshold(1);
    let halt = cpu.run_jit(&mut exec, 1_000);

    assert_eq!(halt, Halt::Halted, "the bare hlt (IF=0) halts the core");
    assert_eq!(
        cpu.reg(0),
        12,
        "rax = 5 + 7 (executed correctly via the JIT)"
    );
    assert_eq!(cpu.reg(1), 7, "rcx = 7");
    // The arithmetic ran as a translated block (the hlt then halts the core via
    // the interpreter fallback — a halting step retires nothing, just like `run`).
    assert!(
        exec.jit_insns >= 3,
        "the mov/mov/add block retired through the JIT (got {})",
        exec.jit_insns
    );
}

// ── Self-modifying-code invalidation ──────────────────────────────────────────

#[test]
fn smc_invalidates_a_cached_block() {
    // A self-rewriting flat-RAM program (paging off, all in page 0). The first
    // block's `add eax, imm8` immediate is patched (by an interpreted `mov byte`,
    // routed through the interpreter write path → the SMC hook) from 5 to 10 on
    // the first pass; a jump-back re-enters the block, which MUST re-translate to
    // the patched bytes. Final eax = 5 (pass 1) + 10 (pass 2) = 15. Without SMC
    // invalidation the cached block would re-add 5 → eax = 10, failing the assert.
    //
    //   0x00  83 C0 05              add eax, 5          (imm byte at 0x02)
    //   0x03  83 F9 01              cmp ecx, 1
    //   0x06  74 0F                 je  0x17            (next 0x08 + 0x0F)
    //   0x08  B9 01 00 00 00        mov ecx, 1
    //   0x0D  C6 04 25 02 00 00 00 0A   mov byte [0x02], 10   (patch the imm)
    //   0x15  EB E9                 jmp 0x00            (next 0x17 - 0x17)
    //   0x17  F4                    hlt
    #[rustfmt::skip]
    let prog: &[u8] = &[
        0x83, 0xC0, 0x05,
        0x83, 0xF9, 0x01,
        0x74, 0x0F,
        0xB9, 0x01, 0x00, 0x00, 0x00,
        0xC6, 0x04, 0x25, 0x02, 0x00, 0x00, 0x00, 0x0A,
        0xEB, 0xE9,
        0xF4,
    ];
    let mut cpu = Cpu::new(64 * 1024);
    cpu.load_at(0, prog);
    // Threshold 1 → the first block is compiled on its first pass, so the patch on
    // the second pass exercises the SMC invalidation + re-translation path.
    let mut exec = X64JitExec::new().with_hot_threshold(1);
    let halt = cpu.run_jit(&mut exec, 10_000);

    assert_eq!(halt, Halt::Halted, "the program halts after two passes");
    assert_eq!(
        cpu.reg(0),
        15,
        "rax = 5 + 10: the cached block was invalidated and re-translated after the \
         self-modifying store patched its immediate (without SMC it would be 10)"
    );
    // Confirm the in-RAM code byte was actually patched (the store landed).
    assert_eq!(
        cpu.vv_ram_read(0x02, 1)[0],
        0x0A,
        "the imm byte was patched to 10"
    );
    // At least one block was re-translated (block @0 compiled twice).
    assert!(
        exec.blocks_translated >= 2,
        "the block at 0x00 was re-translated after the patch (got {})",
        exec.blocks_translated
    );
}

// ── Full JIT boot of a real amd64 Linux to userspace ──────────────────────────

fn vmlinux_elf() -> Vec<u8> {
    let gz = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc44/linux/vmlinux.gz");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(&gz).expect("read cc44 vmlinux.gz")[..])
        .read_to_end(&mut img)
        .expect("gunzip the kernel ELF");
    img
}

#[test]
#[ignore = "boots a real amd64 Linux to userspace via the JIT (~release) — the CC-48 correctness gate"]
fn jit_boots_real_amd64_linux_to_userspace() {
    let kernel = vmlinux_elf();
    // The SAME boot as CC-44 (tests/cc44_x64_boot.rs), but driven by `run_jit` with
    // an X64JitExec instead of the pure interpreter `run`.
    let mut cpu = Cpu::boot_linux(
        1024 * 1024 * 1024,
        &kernel,
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
    );
    let mut exec = X64JitExec::new();

    let start = Instant::now();
    // Drive in bounded slices so a regression can't spin forever; the workload is
    // the byte-identical boot the CC-44 witness runs to a clean power-off.
    let halt = loop {
        match cpu.run_jit(&mut exec, 2_000_000_000) {
            Halt::OutOfBudget => {
                assert!(
                    cpu.insns() < 80_000_000_000,
                    "the JIT boot did not reach userspace + power-off within budget"
                );
            }
            other => break other,
        }
    };
    let elapsed = start.elapsed();

    let console = String::from_utf8_lossy(cpu.console());
    eprintln!("---- guest console ----\n{console}\n---- end ----  (halt: {halt:?})");

    // The kernel reached userspace and ran PID 1, byte-correctly, via the JIT.
    assert!(
        console.contains("Run /init as init process"),
        "the JIT boot handed control to PID 1"
    );
    assert!(
        console.contains("HOLOSPACES-LINUX-USERSPACE-OK"),
        "PID 1 printed its marker (JIT boot reached userspace correctly)"
    );
    assert_eq!(
        halt,
        Halt::Halted,
        "PID 1 powered the machine off (a clean shutdown via hlt)"
    );

    // ── Report: throughput, speedup, JIT coverage ──
    let instrs = cpu.insns();
    let secs = elapsed.as_secs_f64();
    let mips = (instrs as f64) / secs / 1.0e6;
    let total = exec.jit_insns + exec.interp_insns;
    let jit_frac = if total > 0 {
        100.0 * (exec.jit_insns as f64) / (total as f64)
    } else {
        0.0
    };
    // The recorded interpreter baseline (cc48_x64_throughput) is ~15 guest MIPS on
    // a typical CI runner; the printed ratio is the portable signal.
    const INTERP_BASELINE_MIPS: f64 = 15.0;
    println!("\n=== CC-48 JIT boot of real amd64 Linux to userspace ===");
    println!("wall-clock        : {secs:.2}s");
    println!("guest instructions: {instrs}");
    println!("guest throughput  : {mips:.1} MIPS (JIT driver)");
    println!(
        "speedup vs interp : ~{:.2}x (over the ~{:.0} MIPS interpreter baseline)",
        mips / INTERP_BASELINE_MIPS,
        INTERP_BASELINE_MIPS
    );
    println!(
        "JIT coverage      : {jit_frac:.1}% of instructions via JIT blocks \
         ({} JIT / {} interp; {} blocks compiled, {} block runs)",
        exec.jit_insns, exec.interp_insns, exec.blocks_translated, exec.blocks_run
    );
    println!("=======================================================\n");

    assert!(
        instrs > 100_000_000,
        "a real boot retires >100M guest instructions (got {instrs})"
    );
    assert!(
        exec.jit_insns > 0,
        "at least some instructions executed through JIT blocks"
    );
}
