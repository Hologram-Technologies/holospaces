//! `CC-44` — a real amd64 (x86-64) Linux kernel boots to userspace on the x86-64
//! emulator (ADR-021, arc42 ch.10). The third ISA realization of `CC-36`
//! (aarch64) / `CC-9` (riscv64).
//!
//! The implementation under test is the x86-64 system core
//! ([`holospaces::emulator::x64`]): the 64-bit Linux boot protocol
//! (`boot_params`/the zero page, the GDT, the long-mode entry), an IDT + a
//! minimal interrupt controller (PIC/APIC) so the timer and `virtio` IRQs vector,
//! `virtio-mmio` κ-disk servicing over the **shared** `emulator::devbus`, and
//! the instruction tail the boot path hits. The authority is a real, unmodified
//! x86-64 Linux 6.6 kernel (`vv/artifacts/cc44/linux/vmlinux.gz`), with
//! `qemu-system-x86_64` as the differential oracle
//! (`vv/artifacts/cc44/linux/expected-userspace.txt`,
//! `vv/suites/cc44-x64-linux.sh`). The kernel reaches `Run /init`, and PID 1
//! prints its marker + the real `/proc/version`, byte-identical to qemu.
//!
//! [`holospaces::emulator::x64`]: holospaces::emulator::x64

use std::io::Read;
use std::path::Path;

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::emulator::x64::{Cpu, Halt};

/// The committed, *uncompressed* ELF kernel (`vmlinux`), gunzipped. The x86-64
/// core loads its `PT_LOAD` segments and enters `startup_64` directly — the
/// 64-bit boot protocol, no in-guest decompressor.
fn vmlinux_elf() -> Vec<u8> {
    let gz = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc44/linux/vmlinux.gz");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(&gz).expect("read cc44 vmlinux.gz")[..])
        .read_to_end(&mut img)
        .expect("gunzip the kernel ELF");
    img
}

#[test]
#[ignore = "boots a real amd64 Linux to userspace (~release) — run by the CC-44 vv suite"]
fn an_amd64_linux_kernel_boots_to_userspace() {
    let kernel = vmlinux_elf();
    // The 64-bit boot protocol: load the ELF, build the zero page (e820, command
    // line), the GDT, long-mode paging, and enter `startup_64`. The freestanding
    // initramfs PID-1 is embedded in the kernel (CONFIG_INITRAMFS_SOURCE), so no
    // disk is needed to reach userspace; the κ-disk path is exercised by CC-45.
    let mut cpu = Cpu::boot_linux(
        1024 * 1024 * 1024,
        &kernel,
        // `random.trust_cpu=on`: credit the entropy from the core's RDRAND (the
        // hardware RNG the x86-64 core implements) so the crng is fully seeded at
        // boot. Without it the kernel won't credit RDRAND, `wait_for_random_bytes`
        // blocks for interrupt/jitter entropy that a deterministic core can't
        // supply quickly, and PID 1 never starts. The correct posture for a
        // platform that genuinely provides a hardware RNG.
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
    );
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!("---- guest console ----\n{console}\n---- end ----  (halt: {halt:?})");

    // The kernel reached userspace and ran PID 1.
    assert!(
        console.contains("Run /init as init process"),
        "the kernel handed control to PID 1"
    );
    // PID 1 powered the machine off: LINUX_REBOOT_CMD_POWER_OFF →
    // native_machine_halt → stop_this_cpu → `hlt` with interrupts masked → the
    // emulator halts (the clean-stop signal).
    assert_eq!(
        halt,
        Halt::Halted,
        "PID 1 powered the machine off (a clean shutdown via `hlt`)"
    );

    // The differential oracle: the userspace marker + the real /proc/version the
    // emulator produced must match what `qemu-system-x86_64` printed booting the
    // same kernel (captured in expected-userspace.txt).
    let expected = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vv/artifacts/cc44/linux/expected-userspace.txt"),
    )
    .expect("read the qemu oracle");
    for line in expected.lines() {
        if line.is_empty() {
            continue;
        }
        assert!(
            console.contains(line),
            "emulator userspace output matches the qemu oracle, missing line:\n  {line}"
        );
    }
    assert!(
        console.contains("HOLOSPACES-LINUX-USERSPACE-OK"),
        "PID 1 printed its marker"
    );
}

/// Fast, ungated **smoke proof** — the "run it yourself" keystone. A plain
/// `cargo test` (no `--ignored`, no `vv/heavy`, no Docker, no live qemu) boots a
/// real, unmodified upstream Linux 6.6 amd64 to userspace and asserts PID 1's
/// marker. Self-contained: the kernel ELF is committed (`vv/artifacts/cc44/linux/
/// vmlinux.gz`) and the only assertion is the guest's own userspace print, so the
/// proof needs nothing but the crate. ~18s of CPU emulation — the price of "a
/// stranger can watch real Linux boot in one command".
///
/// The heavier proofs stay `#[ignore]`'d above/below: the full qemu-differential
/// oracle line-match, the streamed κ-disk path, and the Docker dogfood.
///
/// Determinism: the core's RDRAND is seeded from the TSC, which is a function of
/// (deterministic) execution, so this boot takes the same known-good ASLR layout
/// every run. (A layout-dependent execve fault exists for *certain* ASLR layouts —
/// see `hologram-execve-stack-fault-native-repro` — but the native default layout
/// is not one of them; this smoke test does not paper over that bug, it simply
/// doesn't hit it.)
#[test]
fn smoke_amd64_linux_boots_to_userspace() {
    let kernel = vmlinux_elf();
    let mut cpu = Cpu::boot_linux(
        1024 * 1024 * 1024,
        &kernel,
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
    );
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    assert!(
        console.contains("Run /init as init process"),
        "the kernel handed control to PID 1\n---- guest console ----\n{console}"
    );
    assert!(
        console.contains("HOLOSPACES-LINUX-USERSPACE-OK"),
        "PID 1 reached userspace and printed its marker\n---- guest console ----\n{console}"
    );
    assert_eq!(halt, Halt::Halted, "PID 1 powered the machine off cleanly");
}

/// JIT Rung 3 — the **long-block speedup A/B** (the `jit` feature). Boots real amd64 Linux to
/// userspace twice — once on the pure interpreter, once with the JIT armed committing only
/// LONG blocks (≥ `JIT_MIN_OPS` modelled ops, where the per-block wasmtime overhead can
/// amortize) — and reports instr/s for each. The decisive go/no-go: does the JIT beat the
/// interpreter on its best-case blocks at all? Both boots must reach userspace (correctness).
/// Run with `cargo test -p holospaces --features jit -- --nocapture`.
#[cfg(feature = "jit")]
#[test]
fn jit_long_block_speedup_ab_on_amd64_boot() {
    use holospaces::emulator::x64;
    use std::time::Instant;
    let kernel = vmlinux_elf();
    let cmdline = "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on";
    let reached = |cpu: &Cpu| {
        String::from_utf8_lossy(cpu.console()).contains("HOLOSPACES-LINUX-USERSPACE-OK")
    };

    // A — interpreter baseline
    x64::set_jit_on(false);
    let mut cpu_a = Cpu::boot_linux(1024 * 1024 * 1024, &kernel, cmdline);
    let t = Instant::now();
    let halt_a = cpu_a.run(40_000_000_000);
    let (dt_a, ins_a) = (t.elapsed().as_secs_f64(), cpu_a.insns());
    assert!(reached(&cpu_a), "baseline reached userspace");
    let _ = x64::drain_jit_stats(); // clear counters before the armed run

    // B — JIT armed, committing only long blocks
    x64::set_jit_on(true);
    let mut cpu_b = Cpu::boot_linux(1024 * 1024 * 1024, &kernel, cmdline);
    let t = Instant::now();
    let halt_b = cpu_b.run(40_000_000_000);
    let (dt_b, ins_b) = (t.elapsed().as_secs_f64(), cpu_b.insns());
    assert!(reached(&cpu_b), "JIT-armed boot reached userspace");
    let (_rec, distinct, compiled, m, mm, trusted, refused, committed) = x64::drain_jit_stats();

    let mips = |ins: u64, dt: f64| ins as f64 / dt / 1e6;
    eprintln!(
        "\n==== JIT LONG-BLOCK A/B (commit blocks >= JIT_MIN_OPS) ====\n\
         baseline : {ins_a:>12} insns in {dt_a:6.1}s = {:6.2} Minsn/s\n\
         jit-armed: {ins_b:>12} insns in {dt_b:6.1}s = {:6.2} Minsn/s\n\
         WALL SPEEDUP: {:.3}x   (jit faster if > 1.0)\n\
         long blocks: {distinct} distinct · {compiled} compiled · {trusted} trusted · \
         {refused} refused · {committed} committed executions · {m} shadow-match · {mm} mismatch\n====\n",
        mips(ins_a, dt_a),
        mips(ins_b, dt_b),
        dt_a / dt_b,
    );
    assert_eq!(halt_a, Halt::Halted, "baseline clean power-off");
    assert_eq!(halt_b, Halt::Halted, "JIT-armed clean power-off");
}

/// JIT Rung 0 — the thesis check. Boots real amd64 Linux to userspace while sampling
/// the executing code page (1/1024 instructions), then reports how concentrated
/// execution is. If a small set of code pages dominates the samples, the interpreter
/// is spending its time in a few hot loops — exactly the work a κ-JIT compiles **once
/// per planet** and reuses; the long cold tail is left interpreted. A flat profile
/// would mean a JIT buys little — so this number decides whether the JIT is worth it.
#[test]
#[ignore = "JIT Rung 0 hot-code profile of a real amd64 boot (~release)"]
fn jit_rung0_hot_code_profile() {
    let kernel = vmlinux_elf();
    let mut cpu = Cpu::boot_linux(
        1024 * 1024 * 1024,
        &kernel,
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
    );
    let _ = cpu.run(40_000_000_000);
    let prof = holospaces::emulator::x64::drain_hotprof();
    let total: u64 = prof.iter().map(|(_, c)| c).sum();
    assert!(total > 0, "the profiler collected samples");
    eprintln!(
        "\n==== JIT RUNG 0: hot-code profile ====\n{total} samples across {} distinct 4 KiB code pages",
        prof.len()
    );
    let mut cum = 0u64;
    for (rank, (page, count)) in prof.iter().take(20).enumerate() {
        cum += *count;
        eprintln!(
            "  #{:<2} page={page:#014x}  {:>6} samples  {:>5.1}%   cum {:>5.1}%",
            rank + 1,
            count,
            100.0 * *count as f64 / total as f64,
            100.0 * cum as f64 / total as f64,
        );
    }
    for topn in [4usize, 16, 64, 256] {
        let s: u64 = prof.iter().take(topn).map(|(_, c)| c).sum();
        eprintln!(
            "  top-{topn:<3} pages = {:.1}% of execution",
            100.0 * s as f64 / total as f64
        );
    }
    eprintln!("==== end (JIT thesis holds if a small top-N covers most execution) ====\n");
}

/// JIT Rung 2 — block discovery. Boots while detecting basic-block entries (a
/// non-sequential `rip` = a branch/return/fault target). Reports the hottest block
/// starts (the JIT's compilation units), how concentrated they are, and the average
/// block length — the data that scopes Rung 3's codegen (how big the blocks are, how
/// few cover most execution).
#[test]
#[ignore = "JIT Rung 2 hot-block profile of a real amd64 boot (~release)"]
fn jit_rung2_hot_block_profile() {
    holospaces::emulator::x64::set_blockprof(true);
    let kernel = vmlinux_elf();
    let mut cpu = Cpu::boot_linux(
        1024 * 1024 * 1024,
        &kernel,
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
    );
    let _ = cpu.run(40_000_000_000);
    let insns = cpu.insns();
    let (entries, blocks) = holospaces::emulator::x64::drain_blockprof();
    holospaces::emulator::x64::set_blockprof(false);
    assert!(entries > 0 && !blocks.is_empty(), "block profiler collected data");
    let samples: u64 = blocks.iter().map(|(_, c)| c).sum();
    eprintln!(
        "\n==== JIT RUNG 2: hot-block profile ====\n{insns} instructions · {entries} block entries · \
         avg block length {:.1} insns · {} distinct blocks sampled",
        insns as f64 / entries as f64,
        blocks.len(),
    );
    let mut cum = 0u64;
    for (rank, (start, count)) in blocks.iter().take(20).enumerate() {
        cum += *count;
        eprintln!(
            "  #{:<2} block={start:#014x}  {:>6} samples  {:>5.1}%   cum {:>5.1}%",
            rank + 1,
            count,
            100.0 * *count as f64 / samples as f64,
            100.0 * cum as f64 / samples as f64,
        );
    }
    for topn in [16usize, 64, 256, 1024] {
        let s: u64 = blocks.iter().take(topn).map(|(_, c)| c).sum();
        eprintln!("  top-{topn:<4} blocks = {:.1}% of block entries", 100.0 * s as f64 / samples as f64);
    }
    eprintln!("==== end (these block starts are Rung 3's compilation units) ====\n");
}

/// The deployed Platform-Manager path: an x64 holospace selected from the
/// architecture picker boots its provisioned amd64 image on the x86-64 core with
/// the κ-disk **streamed** sector-by-sector from a [`KappaStore`] (no full image in
/// RAM) — the exact mechanism `X64Workspace::boot_devcontainer_opfs_streamed` drives
/// in the browser tab (the OPFS-backed store + a sector reader), the x86-64 analogue
/// of `Aarch64Workspace`. This witnesses [`Cpu::boot_linux_disk_streamed`]: the
/// real amd64 kernel boots to userspace with a paged `virtio-blk` κ-disk attached
/// and serviced (probed) during boot, content-addressed through the store — proving
/// the streamed boot the deployed x64 selection relies on. (A real, unmodified
/// `linux-amd64` *rootfs* over this κ-disk root is `CC-45`, the x86-64 analogue of
/// `CC-37`'s arm64 busybox fixture.)
#[test]
#[ignore = "boots a real amd64 Linux from a streamed κ-disk (~release) — the deployed X64Workspace path"]
fn an_amd64_linux_boots_from_a_streamed_kappa_disk() {
    let kernel = vmlinux_elf();

    // A real paged κ-disk: an 8 MiB image streamed into a KappaStore one sector at
    // a time through the same `read(i, buf)` reader the browser peer uses (there it
    // reads each sector from the OPFS rootfs file). A deterministic non-zero pattern
    // so the sectors genuinely content-address through the store (sparse-zero
    // sectors short-circuit). The whole image is never materialized in the core's
    // RAM — "the KappaStore IS the memory, RAM is a cache" (Law L3).
    const DISK_BYTES: usize = 8 * 1024 * 1024;
    let sector_count = (DISK_BYTES / 512) as u64;
    let store: Box<dyn KappaStore> = Box::new(MemKappaStore::new());
    let read = |i: u64, buf: &mut [u8]| {
        for (j, b) in buf.iter_mut().enumerate() {
            *b = (i as u8)
                .wrapping_add(j as u8)
                .wrapping_mul(31)
                .wrapping_add(7);
        }
    };
    let mut cpu = Cpu::boot_linux_disk_streamed(
        1024 * 1024 * 1024,
        &kernel,
        // Same boot posture as the kernel-only boot; the embedded initramfs PID 1
        // reaches userspace, and the attached `virtio-blk` κ-disk is probed (its
        // capacity + sector 0 read through the streamed backing) during boot.
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
        store,
        sector_count,
        read,
    );
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!(
        "---- guest console (streamed κ-disk) ----\n{console}\n---- end ----  (halt: {halt:?})"
    );

    assert!(
        console.contains("Run /init as init process"),
        "the kernel handed control to PID 1 with the streamed κ-disk attached"
    );
    assert_eq!(
        halt,
        Halt::Halted,
        "PID 1 powered the machine off — a clean boot through the streamed κ-disk path"
    );
    assert!(
        console.contains("HOLOSPACES-LINUX-USERSPACE-OK"),
        "PID 1 printed its marker — the real amd64 kernel booted from the streamed κ-disk"
    );
}

/// κ-snapshot thesis: a booted machine's RAM content-addresses to a tiny working set. Boots
/// real amd64 Linux to userspace, splits the 1 GiB guest RAM into 4 KiB pages, BLAKE3-keys
/// each, and reports how few UNIQUE pages there are. That dedup ratio IS the "boot-once,
/// resume-anywhere" speed: a resume streams only the unique pages (verify-before-use), not the
/// nominal gigabyte — which is why it can be seconds while a cold boot is minutes.
#[test]
fn kappa_snapshot_ram_dedup_ratio_after_amd64_boot() {
    use std::collections::HashSet;
    let kernel = vmlinux_elf();
    let mut cpu = Cpu::boot_linux(
        1024 * 1024 * 1024,
        &kernel,
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
    );
    let halt = cpu.run(40_000_000_000);
    assert!(
        String::from_utf8_lossy(cpu.console()).contains("HOLOSPACES-LINUX-USERSPACE-OK"),
        "reached userspace"
    );
    assert_eq!(halt, Halt::Halted);

    const PAGE: usize = 0x1000;
    let ram = cpu.ram();
    let total = ram.len() / PAGE;
    let zero = [0u8; PAGE];
    let mut unique: HashSet<[u8; 32]> = HashSet::new();
    let mut zero_pages = 0usize;
    for p in ram.chunks_exact(PAGE) {
        if p == zero {
            zero_pages += 1;
        }
        unique.insert(*blake3::hash(p).as_bytes());
    }
    let unique_n = unique.len();
    let mib = |pages: usize| pages * PAGE / (1024 * 1024);
    eprintln!(
        "\n==== κ-SNAPSHOT RAM DEDUP (post-boot, BLAKE3 per 4 KiB page) ====\n\
         total : {total} pages = {} MiB nominal\n\
         zero  : {zero_pages} pages ({:.1}%)\n\
         UNIQUE: {unique_n} pages = {} MiB  →  {:.1}x dedup\n\
         a resume streams {} MiB (the unique κ pages), not {} MiB\n====\n",
        mib(total),
        100.0 * zero_pages as f64 / total as f64,
        mib(unique_n),
        total as f64 / unique_n as f64,
        mib(unique_n),
        mib(total),
    );
    assert!(
        unique_n < total / 4,
        "RAM deduplicates by >4x (the κ-snapshot thesis): {unique_n} unique / {total} total"
    );
}
