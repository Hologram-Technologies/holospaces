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

/// BISECT (x64 graphical desktop, init-crash hunt): boot the current cc44 kernel and report whether
/// PID 1 runs CLEAN (prints the freestanding marker `HOLOSPACES-LINUX-USERSPACE-OK`) or CRASHES
/// (`Attempted to kill init`). Used to isolate the userspace-init SIGSEGV: build a gcc-15 NO-FB kernel
/// → if this reports CLEAN, the framebuffer config breaks init; if it CRASHES, the gcc-15 build does.
/// Run: `cargo test -p holospaces --release --test cc44_x64_boot bisect_does_init_run_clean -- --ignored --nocapture`
#[test]
#[ignore = "init-crash bisect: boots the current cc44 kernel at several RAM sizes, reports CLEAN vs CRASH"]
fn bisect_does_init_run_clean() {
    let kernel = vmlinux_elf();
    // The crash is a divide-by-zero in blk_mq_alloc_map_and_rqs (RBX=0). cc44 ran blk_mq fine at 1 GiB;
    // all crashes seen at 512 MiB → test the RAM size directly.
    for ram_mb in [512u64, 1024, 256, 2048] {
        let mut cpu = Cpu::boot_linux(
            (ram_mb * 1024 * 1024) as usize,
            &kernel,
            "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
        );
        let mut outcome = "TIMEOUT";
        for _ in 0..40 {
            let halt = cpu.run(100_000_000);
            let console = String::from_utf8_lossy(cpu.console());
            if console.contains("HOLOSPACES-LINUX-USERSPACE-OK") {
                outcome = "CLEAN (init ran, marker printed)";
                break;
            }
            if console.contains("Attempted to kill init") || console.contains("divide error") {
                outcome = "CRASH (blk_mq divide-by-zero → kill init)";
                break;
            }
            if matches!(halt, Halt::Halted) {
                outcome = "HALTED (no marker)";
                break;
            }
        }
        eprintln!("==== RAM {ram_mb} MiB → {outcome} ====");
    }
}

/// G3 (x64 graphical desktop): the rebuilt kernel (DRM_SIMPLEDRM + fbcon) BINDS the passive linear
/// framebuffer the emulator advertises via `screen_info`, and its console renders PIXELS into it.
/// Proven natively — fbcon binds during kernel init (before PID 1), so the serial log shows the bind
/// message and `read_framebuffer()` is non-zero, no browser needed. The foundation for the desktop.
/// Run: `cargo test -p holospaces --release --test cc44_x64_boot the_kernel_binds_a_graphical_framebuffer -- --ignored --nocapture`
#[test]
#[ignore = "needs the rebuilt FB kernel (DRM_SIMPLEDRM+fbcon); proves the framebuffer binds + renders"]
fn the_kernel_binds_a_graphical_framebuffer() {
    let kernel = vmlinux_elf();
    // `console=tty0` (alongside ttyS0) so fbcon drives the VT onto the framebuffer; the fbcon-bind
    // message is printk (reaches BOTH consoles), so the serial log still witnesses it.
    let mut cpu = Cpu::boot_linux(
        512 * 1024 * 1024,
        &kernel,
        "earlyprintk=serial,ttyS0 console=ttyS0 console=tty0 random.trust_cpu=on",
    );
    let mut fbcon_bound = false;
    let mut max_pixels = 0usize;
    let mut first_pixel_chunk = -1i32;
    // Progress trace so a watcher can see crawl-vs-hang in real time (last serial line + length).
    let progress = std::env::var("HOLO_FB_PROGRESS").ok();
    for chunk in 0..1500 {
        let halt = cpu.run(100_000_000);
        let console = String::from_utf8_lossy(cpu.console());
        if let Some(p) = &progress {
            let last = console.lines().last().unwrap_or("");
            let _ = std::fs::write(
                p,
                format!("chunk={chunk} steps={}M serial_len={} fbcon={fbcon_bound} pixels={max_pixels}\nlast: {last}\n",
                    (chunk + 1) * 100, console.len()),
            );
        }
        // The canonical fbcon / simpledrm bind messages.
        fbcon_bound = fbcon_bound
            || console.contains("frame buffer device")
            || console.contains("simple-framebuffer")
            || console.contains("simpledrm")
            || console.contains("Console: switching to colour");
        // Peak non-zero pixels DURING the run (robust to a blank on power-off).
        let fb = cpu.read_framebuffer();
        let nz = fb.iter().step_by(16).filter(|&&b| b != 0).count();
        if nz > 0 && first_pixel_chunk < 0 {
            first_pixel_chunk = chunk;
        }
        max_pixels = max_pixels.max(nz);
        if matches!(halt, Halt::Halted) {
            break;
        }
        // Solid witness — fbcon bound and the console drew pixels; stop early.
        if fbcon_bound && max_pixels > 0 && first_pixel_chunk >= 0 && chunk > first_pixel_chunk + 2 {
            break;
        }
    }
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!(
        "---- serial tail ----\n{}\n---- end ----",
        console.lines().rev().take(40).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n"),
    );
    eprintln!(
        "\n==== G3 FRAMEBUFFER WITNESS ====\n  fbcon/simpledrm bound: {fbcon_bound}\n  peak framebuffer pixels (non-zero): {max_pixels}\n  ({}×{} = {} px)\n====\n",
        Cpu::FB_W, Cpu::FB_H, Cpu::FB_W * Cpu::FB_H,
    );
    assert!(fbcon_bound, "the kernel bound fbcon/simpledrm to the advertised framebuffer (serial log)");
    assert!(max_pixels > 0, "the console rendered live pixels into the framebuffer");
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

/// κ-snapshot Step 1 GATE — a mid-boot snapshot/restore is **bit-exact**. Boots real amd64
/// Linux partway (a live, running machine), snapshots it with [`Cpu::snapshot`], restores it
/// into a FRESH `Cpu` with [`Cpu::restore`], then runs BOTH the original and the restored
/// machine forward to the userspace marker — their consoles must be byte-identical and both
/// power off cleanly. Any omitted CPU/segment/device/timer/interrupt state would diverge the
/// resumed guest, so a green run proves the snapshot captures the *whole* machine. This is the
/// serialization the κ-snapshot path content-addresses (1 GiB → 44 MiB unique, measured) and
/// streams to resume in seconds instead of re-running the multi-minute boot.
#[test]
fn kappa_snapshot_midboot_restore_is_bit_exact_to_userspace() {
    const MARKER: &str = "HOLOSPACES-LINUX-USERSPACE-OK";
    let kernel = vmlinux_elf();
    let cmdline = "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on";

    // Boot partway — a live, mid-boot machine (kernel running, NOT yet at userspace).
    let mut orig = Cpu::boot_linux(1024 * 1024 * 1024, &kernel, cmdline);
    let mid = orig.run(200_000_000);
    assert_eq!(mid, Halt::OutOfBudget, "the snapshot point is mid-boot (still running)");
    assert!(
        !String::from_utf8_lossy(orig.console()).contains(MARKER),
        "userspace has NOT been reached yet at the snapshot point"
    );

    // Snapshot the running machine; restore into a fresh, small core (restore resizes RAM +
    // flushes the lazily-rebuilt TLB/ifetch caches).
    let snap = orig.snapshot();
    eprintln!(
        "\n==== κ-SNAPSHOT mid-boot: {} MiB flat snapshot @ {} insns ====",
        snap.len() / (1024 * 1024),
        orig.insns()
    );
    let mut resumed = Cpu::new(0x1000);
    assert!(resumed.restore(&snap), "restore accepts the snapshot");
    drop(snap); // free ~1 GiB before running two full machines forward

    // Run BOTH forward to userspace — a bit-exact resume stays identical the whole way.
    let h_orig = orig.run(40_000_000_000);
    let h_resumed = resumed.run(40_000_000_000);
    let c_orig = String::from_utf8_lossy(orig.console()).into_owned();
    let c_resumed = String::from_utf8_lossy(resumed.console()).into_owned();

    assert!(
        c_resumed.contains(MARKER),
        "the RESUMED machine reached userspace from the snapshot\n---- resumed console ----\n{c_resumed}"
    );
    assert!(c_orig.contains(MARKER), "the original machine reached userspace");
    assert_eq!(h_orig, Halt::Halted, "original powered off cleanly");
    assert_eq!(h_resumed, Halt::Halted, "resumed powered off cleanly");
    assert_eq!(
        c_orig, c_resumed,
        "the resumed machine's console is BYTE-IDENTICAL to the original — bit-exact resume"
    );
    eprintln!(
        "==== κ-SNAPSHOT resume BIT-EXACT: original ≡ resumed to userspace ({} console bytes) ====\n",
        c_orig.len()
    );
}

/// κ-snapshot Step 2 GATE — **content-addressed** resume on a real boot. Boots amd64 Linux
/// partway, snapshots it with [`Cpu::snapshot_kappa`] (RAM → a BLAKE3 4 KiB page manifest whose
/// UNIQUE pages dedup into a `KappaStore`), reports the real dedup (unique MiB vs 1 GiB — the
/// thesis number), then resumes into a FRESH `Cpu` with [`Cpu::restore_kappa`] (every page
/// verified before use, L5) and drives BOTH machines to userspace — consoles byte-identical.
/// A resume streams only the unique pages held in the store, not the nominal 1 GiB.
#[test]
fn kappa_snapshot_kappa_resume_to_userspace() {
    const MARKER: &str = "HOLOSPACES-LINUX-USERSPACE-OK";
    const PAGE: usize = 0x1000;
    let kernel = vmlinux_elf();
    let cmdline = "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on";

    let mut orig = Cpu::boot_linux(1024 * 1024 * 1024, &kernel, cmdline);
    assert_eq!(orig.run(200_000_000), Halt::OutOfBudget, "snapshot point is mid-boot");

    // Content-address the machine: RAM pages dedup into the store automatically.
    let store = MemKappaStore::new();
    let snap = orig.snapshot_kappa(&store).expect("snapshot_kappa");
    let total = snap.page_count();
    let unique = store.approximate_count();
    let mib = |pages: usize| pages * PAGE / (1024 * 1024);
    eprintln!(
        "\n==== κ-SNAPSHOT (content-addressed) @ {} insns ====\n\
         total : {total} pages = {} MiB\n\
         UNIQUE: {unique} pages = {} MiB in the KappaStore  →  {:.1}x dedup\n\
         a κ-resume streams {} MiB (+ {} B state), not {} MiB\n====\n",
        orig.insns(),
        mib(total),
        mib(unique),
        total as f64 / unique as f64,
        mib(unique),
        snap.state_len(),
        mib(total),
    );

    // Resume BY κ into a fresh core — each page fetched from the store + verified (L5).
    let mut resumed = Cpu::new(0x1000);
    assert!(resumed.restore_kappa(&snap, &store), "κ-resume verifies every page + reconstructs");

    // Both machines run to userspace and stay byte-identical.
    let h_orig = orig.run(40_000_000_000);
    let h_resumed = resumed.run(40_000_000_000);
    let c_orig = String::from_utf8_lossy(orig.console()).into_owned();
    let c_resumed = String::from_utf8_lossy(resumed.console()).into_owned();

    assert!(
        c_resumed.contains(MARKER),
        "the κ-RESUMED machine reached userspace\n---- resumed console ----\n{c_resumed}"
    );
    assert!(c_orig.contains(MARKER), "the original reached userspace");
    assert_eq!(h_orig, Halt::Halted);
    assert_eq!(h_resumed, Halt::Halted, "κ-resumed machine powered off cleanly");
    assert_eq!(
        c_orig, c_resumed,
        "the κ-resumed console is BYTE-IDENTICAL to the original — bit-exact content-addressed resume"
    );
    assert!(unique < total / 4, "RAM deduplicates by >4x in the store ({unique}/{total})");
    eprintln!("==== κ-RESUME BIT-EXACT from {} unique pages ====\n", unique);
}

/// **"Resume, don't re-run" — the headline of the speed architecture (Layer 1).** Boots a real
/// amd64 Linux to userspace (timed), content-addresses the *running* machine into a κ-snapshot,
/// then resumes it into a FRESH core (timed) and shows the resumed machine is **already at
/// userspace** — the console marker is present without executing a single guest instruction. The
/// resume is page-reconstruct + verify (L5), not a re-boot, so it is dramatically faster than the
/// cold boot. This is why opening a warm κ feels instant: nobody re-runs the boot (or the `apk`)
/// — they resume the state the planet computed once.
#[test]
#[ignore = "measures resume-vs-boot (release boot is ~tens of seconds) — run explicitly"]
fn kappa_resume_lands_at_userspace_instantly() {
    use std::time::Instant;
    const MARKER: &str = "HOLOSPACES-LINUX-USERSPACE-OK";
    let kernel = vmlinux_elf();
    let cmdline = "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on";

    // COLD BOOT to userspace, timed — run in chunks so we snapshot a LIVE machine the moment it
    // reaches the marker (not a halted one).
    let t0 = Instant::now();
    let mut orig = Cpu::boot_linux(512 * 1024 * 1024, &kernel, cmdline);
    loop {
        let h = orig.run(50_000_000);
        if String::from_utf8_lossy(orig.console()).contains(MARKER) || h == Halt::Halted {
            break;
        }
    }
    let boot_time = t0.elapsed();
    assert!(
        String::from_utf8_lossy(orig.console()).contains(MARKER),
        "the cold boot reached userspace"
    );

    // SNAPSHOT the running machine at userspace (content-addressed; unique pages dedup).
    let store = MemKappaStore::new();
    let snap = orig.snapshot_kappa(&store).expect("snapshot_kappa at userspace");
    let (total, unique) = (snap.page_count(), store.approximate_count());

    // RESUME into a fresh core, timed — verify every page (L5) + reconstruct. No guest execution.
    let t1 = Instant::now();
    let mut resumed = Cpu::new(0x1000);
    assert!(resumed.restore_kappa(&snap, &store), "κ-resume verifies every page (L5) + reconstructs");
    let resume_time = t1.elapsed();

    // The decisive property: the resumed machine is ALREADY at userspace — the marker is in its
    // restored console without running anything. Resume landed AT the state, it did not re-run.
    assert!(
        String::from_utf8_lossy(resumed.console()).contains(MARKER),
        "the RESUMED machine is already at userspace — resume reconstructs the state, no re-boot"
    );

    let speedup = boot_time.as_secs_f64() / resume_time.as_secs_f64().max(1e-9);
    eprintln!(
        "\n==== RESUME, DON'T RE-RUN (Layer 1) ====\n\
         cold boot to userspace : {boot_time:>10.2?}\n\
         resume from κ-snapshot : {resume_time:>10.2?}   →  {speedup:.0}x faster, lands at userspace\n\
         streamed {unique} unique pages (of {total}) + state — never the nominal RAM\n====\n",
    );
    assert!(resume_time < boot_time, "resume is faster than a cold boot ({resume_time:?} vs {boot_time:?})");
}

/// Fixture generator for the browser/node resume witness (Step 4): boots real amd64 Linux far
/// enough to produce console, snapshots the WHOLE machine, and writes the snapshot + the expected
/// console next to the wasm harness. The node witness (`x64-resume-test.mjs`) then loads these,
/// `X64Workspace.resume`s the snapshot in the compiled wasm, and confirms the console comes back
/// bit-exact and the machine continues — a *running* userspace from a snapshot, no boot in the tab.
/// Run explicitly: `cargo test -p holospaces --test cc44_x64_boot generate_x64_resume_fixture -- --ignored --nocapture`.
#[test]
#[ignore = "fixture generator — writes a host snapshot for the browser/node resume witness"]
fn generate_x64_resume_fixture() {
    use std::io::Write;
    let kernel = vmlinux_elf();
    // 128 MiB is plenty for early boot; smaller snapshot = a light fixture for the node harness.
    let mut cpu = Cpu::boot_linux(
        128 * 1024 * 1024,
        &kernel,
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
    );
    assert_eq!(cpu.run(120_000_000), Halt::OutOfBudget, "snapshot a live, mid-boot machine");
    let console = String::from_utf8_lossy(cpu.console()).into_owned();
    assert!(console.len() > 32, "captured real early-boot console ({} bytes)", console.len());
    let snap = cpu.snapshot();

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../holospaces-web/web/fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::File::create(dir.join("x64-resume-snapshot.bin"))
        .unwrap()
        .write_all(&snap)
        .unwrap();
    std::fs::File::create(dir.join("x64-resume-console.txt"))
        .unwrap()
        .write_all(console.as_bytes())
        .unwrap();
    eprintln!(
        "==== x64 RESUME FIXTURE: {} MiB snapshot, {} B console @ {} insns → {} ====",
        snap.len() / (1024 * 1024),
        console.len(),
        cpu.insns(),
        dir.display()
    );
}
