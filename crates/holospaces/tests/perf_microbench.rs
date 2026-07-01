//! PERF micro-bench (dev-only, `#[ignore]`) — a seconds-fast, repeatable IPS probe for the
//! interpreter hot path, so performance work isn't gated on 10-minute python boots.
//!
//! Resumes the warm Alpine `.holo` shell (same fixture as CC-62) and feeds a tight, CPU-bound,
//! flag-heavy userspace loop (busybox `awk` — an interpreter loop, representative of the heavy
//! images the perf milestone targets: python/node/glibc). Reports instructions, wall-clock, derived
//! Minsn/s, and the opcode-frequency histogram (where the instructions actually go).
//!
//! Run: `cargo test --release -p holospaces --test perf_microbench -- --ignored --nocapture`

use std::path::Path;
use std::time::Instant;

use holospaces::emulator::x64::{drain_ophist, reset_ophist, Cpu};

fn kblob() -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../holospaces-web/web/fixtures/x64-alpine-shell.kblob"),
    )
    .expect("read warm x64 shell kblob")
}
fn console(cpu: &Cpu) -> String {
    String::from_utf8_lossy(cpu.console()).into_owned()
}

/// Feed one command, run until the `holo$ ` prompt returns (generous budget for a compute loop),
/// returning (insns_delta, wall_seconds).
fn timed(cpu: &mut Cpu, cmd: &str) -> (u64, f64) {
    let before = console(cpu).len();
    let mut bytes = cmd.as_bytes().to_vec();
    bytes.push(b'\n');
    cpu.feed_console(&bytes);
    let i0 = cpu.insns();
    let t = Instant::now();
    let mut done = false;
    for _ in 0..1500 {
        let _ = cpu.run(1_000_000);
        let c = console(cpu);
        if c.len() > before && c[before..].matches("holo$ ").count() >= 1 {
            done = true;
            break;
        }
    }
    assert!(done, "command did not return to prompt within budget");
    (cpu.insns() - i0, t.elapsed().as_secs_f64())
}

#[test]
#[ignore = "perf probe: resumes the warm shell + runs a compute loop (seconds)"]
fn interp_ips_microbench() {
    let mut cpu = Cpu::new(0x1000);
    assert!(cpu.restore_kappa_blob(&kblob()), "restore warm x64 shell");
    for _ in 0..20 {
        cpu.run(2_000_000);
    }
    // Warm/prime the shell with a trivial command so the first real measurement isn't skewed by
    // demand-paging awk's text in.
    let _ = timed(&mut cpu, "awk 'BEGIN{print 1}'");

    // The measured workload: a tight awk arithmetic loop — pure userspace ALU + the awk bytecode
    // dispatch, no fork/exec, no I/O. Deterministic result (sum of i for i in 0..N-1).
    let n = 100_000u64;
    let cmd = format!("awk 'BEGIN{{s=0;for(i=0;i<{n};i++)s+=i;print s}}'");
    let want = (n * (n - 1) / 2).to_string();

    // Warm up once (page awk's text in, warm caches/TLB) so measurements aren't skewed by cold start.
    let (insns, _) = timed(&mut cpu, &cmd);
    assert!(console(&cpu).contains(&want), "warm-up awk sum wrong");

    // Measure each mode REPEATED, take the MIN wall (least affected by transient contention). Interleave
    // off/on to cancel any slow drift. `reset_ophist` arms profiling; leaving it unset = production path.
    let mut best_off = f64::INFINITY;
    let mut best_on = f64::INFINITY;
    let (mut prim, mut sec) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        let (_i, w_off) = timed(&mut cpu, &cmd); // production: ophist OFF
        best_off = best_off.min(w_off);
        reset_ophist();
        let (_j, w_on) = timed(&mut cpu, &cmd); // profiling: ophist ON
        best_on = best_on.min(w_on);
        let (p, s) = drain_ophist();
        if !p.is_empty() {
            (prim, sec) = (p, s);
        }
    }
    let ok = console(&cpu).contains(&want);
    let mips = |dt: f64| insns as f64 / dt / 1e6;
    eprintln!(
        "\n==== PERF micro-bench (awk loop, N={n}, {insns} insns/run, best-of-3) ====\n\
         result-correct: {ok}\n\
         PROD  (ophist off): {best_off:.3}s => {:.2} Minsn/s\n\
         PROF  (ophist on) : {best_on:.3}s => {:.2} Minsn/s\n\
         ophist per-insn overhead: {:.1}%  (on vs off)\n",
        mips(best_off),
        mips(best_on),
        (best_on / best_off - 1.0) * 100.0,
    );
    eprintln!("top primary opcodes (op:count):");
    for (op, c) in prim.iter().take(20) {
        eprintln!("  {op:#04x}: {c}");
    }
    eprintln!("top 0F-secondary opcodes (op:count):");
    for (op, c) in sec.iter().take(12) {
        eprintln!("  0f {op:02x}: {c}");
    }
    assert!(ok, "awk loop produced the wrong sum — correctness broke");
}
