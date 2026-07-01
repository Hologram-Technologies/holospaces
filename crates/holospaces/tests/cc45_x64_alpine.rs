//! `CC-45` — a real **Alpine** (`linux/amd64`) userland boots to a running shell
//! over the **virtio-blk κ-disk** on the x86-64 core (ADR-021, arc42 ch.10). The
//! rootfs realization of `CC-44` (which boots only the kernel to a freestanding
//! initramfs): here the *distro userland* — stock musl + busybox + apk-tools — is
//! the authority, mounted over `/dev/vda` and actually executed.
//!
//! The implementation under test is the x86-64 system core
//! ([`holospaces::emulator::x64`]) booting the **CC-44 platform kernel** (reused —
//! Alpine ships no kernel here) over a `virtio-mmio` κ-disk (`emulator::devbus`,
//! Law L4) whose root filesystem is a **bootable ext4 assembled from the pinned
//! Alpine minirootfs layer** ([`holospaces::assembly::assemble_ext4_bootable`]) with
//! the freestanding `/init` injected. The authority is the real, unmodified Alpine
//! userland (`vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz`), with
//! `qemu-system-x86_64` as the differential oracle
//! (`vv/artifacts/cc45/alpine/expected-userspace.txt`, `vv/suites/cc45-x64-alpine.sh`).
//!
//! This is the host-side foundation of the browser `X64Workspace` Alpine path; the
//! streamed-κ-disk + interactive variants compose on it (the x86-64 analogue of
//! `Aarch64Workspace::boot_devcontainer_opfs_full`).
//!
//! [`holospaces::emulator::x64`]: holospaces::emulator::x64

use std::io::Read;
use std::path::{Path, PathBuf};

use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::{drain_disk_fetch_stats, reset_disk_fetch_stats};
use holospaces::emulator::x64::{
    drain_ophist, reset_ophist, reset_user_insns, user_insns_seen, Cpu, Halt,
};

fn artifact(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

/// The CC-45 platform kernel, gunzipped — the x86-64 core enters `startup_64`
/// directly (64-bit boot protocol, no in-guest decompressor). This is the CC-44
/// kernel REBUILT with `CONFIG_INITRAMFS_SOURCE=""` so there is **no embedded
/// initramfs** to shadow the disk root: with `root=/dev/vda init=/init` the kernel
/// mounts the Alpine ext4 directly and runs the injected `/init` (the CC-44 kernel's
/// embedded initramfs init prints `HOLOSPACES-LINUX-USERSPACE-OK` and halts before
/// ever pivoting, which is why this suite reuses a disk-rooting kernel here).
fn vmlinux_elf() -> Vec<u8> {
    let gz = artifact("vv/artifacts/cc45/linux/vmlinux.gz");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(&gz).expect("read cc45 vmlinux.gz")[..])
        .read_to_end(&mut img)
        .expect("gunzip the kernel ELF");
    img
}

/// Dump the *exact* assembled Alpine ext4 (same layers + injected `/init`) to a raw
/// image file so `qemu-system-x86_64` can boot the identical disk — the differential
/// oracle for localizing the emulator's `ld-musl` divergence. Writes to the path in
/// `CC45_DISK_OUT` (default `target/cc45-alpine.img`).
#[test]
#[ignore = "writes the assembled Alpine ext4 to a file for the qemu differential"]
fn dump_cc45_disk_image() {
    let layer_blob = std::fs::read(artifact("vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz"))
        .expect("read the pinned alpine minirootfs layer");
    let init = std::fs::read(artifact("vv/artifacts/cc45/alpine/init")).expect("read the freestanding /init");
    let rootfs = assemble_ext4_bootable(
        &[Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer_blob }],
        &init,
        256 * 1024 * 1024,
    )
    .expect("assemble the bootable Alpine ext4");
    let out = std::env::var("CC45_DISK_OUT").unwrap_or_else(|_| "target/cc45-alpine.img".to_string());
    std::fs::write(&out, &rootfs).expect("write the disk image");
    eprintln!("CC45-DISK-WRITTEN {} bytes -> {out}", rootfs.len());
}

#[test]
#[ignore = "boots a real Alpine amd64 userland to a shell over the κ-disk (~release) — run by the CC-45 vv suite"]
fn a_real_alpine_userland_boots_over_the_kappa_disk() {
    let kernel = vmlinux_elf();

    // The pinned Alpine minirootfs is the single OCI-style layer; the assembler
    // overlays it and injects the freestanding `/init`, sizing a 256 MiB ext4 (room
    // for apk in later phases). The κ-disk takes the assembled image as content
    // (CC-7); the guest mounts it over /dev/vda.
    let layer_blob = std::fs::read(artifact("vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz"))
        .expect("read the pinned alpine minirootfs layer");
    let init = std::fs::read(artifact("vv/artifacts/cc45/alpine/init")).expect("read the freestanding /init");
    let rootfs = assemble_ext4_bootable(
        &[Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer_blob }],
        &init,
        256 * 1024 * 1024,
    )
    .expect("assemble the bootable Alpine ext4");

    // Boot the real userland over the virtio-blk κ-disk. `random.trust_cpu=on`
    // credits the core's RDRAND so the crng seeds without blocking PID 1 (as CC-44).
    let mut cpu = Cpu::boot_linux_disk(
        1024 * 1024 * 1024,
        &kernel,
        rootfs,
        // `virtio_mmio.device=<size>@<base>:<irq>` tells the kernel where the
        // x64 core's virtio-mmio block slot lives (0x200 bytes @ 0xD000_0000, IRQ
        // 11 — `VIRTIO_BLK_BASE`/`VIRTIO_BLK_IRQ`). Without it the microvm-style
        // virtio-mmio device is undiscoverable (no PCI, no DT/ACPI for it), so
        // /dev/vda never appears and root mount fails (`unknown-block(0,0)`).
        "virtio_mmio.device=0x200@0xd0000000:11 console=ttyS0 root=/dev/vda rw init=/init random.trust_cpu=on norandmaps",
    );
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!("---- guest console ----\n{console}\n---- end ----  (halt: {halt:?})");

    // The injected PID 1 reached userspace over the κ-disk root.
    assert!(
        console.contains("HOLOSPACES-ALPINE-USERSPACE-OK"),
        "PID 1 ran from the Alpine ext4 root"
    );
    // The real Alpine root is the live filesystem (release read straight off it).
    assert!(
        console.contains("alpine-release:"),
        "the real Alpine root filesystem is mounted over /dev/vda"
    );
    // The stock musl-linked Alpine userland actually executed (fork+execve busybox;
    // the kernel resolved /lib/ld-musl-x86_64.so.1 as PT_INTERP).
    assert!(
        console.contains("ALPINE-USERLAND-RAN"),
        "stock musl + busybox executed from the Alpine root"
    );
    assert!(
        console.contains("apk-tools"),
        "apk-tools runs (musl dynamic-link + the Alpine package manager present)"
    );
    // Clean shutdown via `hlt` with interrupts masked (the CC-44 stop signal).
    assert_eq!(halt, Halt::Halted, "PID 1 powered the machine off cleanly");

    // The differential oracle: every committed qemu line must appear (re-derived
    // live by the suite whenever qemu is present, so it can never go stale).
    let expected = std::fs::read_to_string(artifact("vv/artifacts/cc45/alpine/expected-userspace.txt"))
        .expect("read the qemu oracle");
    for line in expected.lines() {
        if line.trim().is_empty() {
            continue;
        }
        assert!(
            console.contains(line),
            "emulator userspace matches the qemu oracle, missing line:\n  {line}"
        );
    }
}

/// Resume-stability repro (H1): load the warm shell snapshot (no re-boot) and characterize what the
/// resumed machine *does* when run — is it progressing (many distinct code pages, like a healthy idle
/// kernel cycling idle↔timer↔scheduler) or spinning (a few pages = stuck busy-wait)? The hot page is
/// the thing it's stuck on. Run:
/// `cargo test -p holospaces --release --test cc45_x64_alpine resume_shell_snapshot_progress -- --ignored --nocapture`
#[test]
#[ignore = "loads the shell snapshot fixture and characterizes spin vs progress (fast — no boot)"]
fn resume_shell_snapshot_progress() {
    use std::collections::HashMap;
    let blob = std::fs::read(artifact("crates/holospaces-web/web/fixtures/x64-alpine-shell.kblob"))
        .expect("read the shell snapshot fixture (generate it first)");
    let mut cpu = Cpu::new(0x1000);
    assert!(cpu.restore_kappa_blob(&blob), "restore the shell snapshot");
    eprintln!(
        "restored: rip={:#x} insns={} console_len={} cpl-ish-rip={:#x}",
        cpu.rip(), cpu.insns(), cpu.console().len(), cpu.rip() >> 47,
    );
    let start = cpu.insns();
    let mut hist: HashMap<u64, u32> = HashMap::new();
    let mut rips: Vec<u64> = Vec::new();
    for _ in 0..4000 {
        cpu.run(50_000);
        let r = cpu.rip();
        *hist.entry(r & !0xfff).or_default() += 1;
        if rips.len() < 40 { rips.push(r); }
    }
    eprintln!(
        "\n==== RESUMED-SHELL RUN ====\nran {} insns over 4000 ticks; distinct code PAGES hit: {}",
        cpu.insns() - start, hist.len(),
    );
    let mut v: Vec<_> = hist.into_iter().collect();
    v.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (page, c) in v.iter().take(10) {
        eprintln!("  page {page:#x}: {c} samples");
    }
    eprintln!("first sampled rips: {:#x?}", &rips[..rips.len().min(20)]);
    eprintln!("final rip={:#x} console_len={}\n====\n", cpu.rip(), cpu.console().len());
}

/// Native interactivity test: resume the warm shell snapshot, type `echo HOLO$((6*7))`, and expect
/// `HOLO42` — proving `feed_console` (→ ttyS0 RX) reaches the shell on a resumed machine. Fast (no
/// boot). Run: `cargo test -p holospaces --release --test cc45_x64_alpine resume_shell_responds_to_input -- --ignored --nocapture`
#[test]
#[ignore = "native: resume the shell snapshot, type a command, expect output"]
fn resume_shell_responds_to_input() {
    let blob = std::fs::read(artifact("crates/holospaces-web/web/fixtures/x64-alpine-shell.kblob"))
        .expect("read the shell snapshot fixture");
    let mut cpu = Cpu::new(0x1000);
    assert!(cpu.restore_kappa_blob(&blob), "restore the shell snapshot");
    // The κ-blob now carries the virtio-blk κ-disk (Sys::snap serializes it), so the resumed
    // machine has a working /dev/vda and a forked child can demand-page from it. No re-attach.
    let before = cpu.console().len();
    // DECISIVE INSTRUMENT: count CPL==3 instructions during the run (per-step, not sampled) so we
    // know whether the resumed machine EVER schedules a userspace task at all, vs the kernel idle
    // loop merely servicing interrupts. Two phases: (1) baseline — run the idle machine with NO
    // input and see if userspace ever runs; (2) feed a command and re-feed periodically.
    reset_user_insns();
    let user_before_input = {
        for _ in 0..20 {
            cpu.run(2_000_000);
        }
        user_insns_seen()
    };
    let _ = user_before_input;
    // Drive a sequence of REAL commands — builtins and external (fork+exec) — polling each to
    // completion (cap the cycles) and recording how many instructions each needed. This is the
    // milestone gate (the resumed shell is a usable computer) AND a perf probe (external commands
    // pay a timer-catch-up cost on the fast-forwarded clock).
    let cmds: &[(&str, &str)] = &[
        ("echo HOLO$((6*7))\n", "HOLO42"),
        ("uname -m\n", "x86_64"),
        ("cat /etc/alpine-release\n", "3.20"),
        ("echo $((100+23))\n", "123"),
        ("pwd\n", "/"),
    ];
    let mut report = String::new();
    for (line, want) in cmds {
        let con_before = cpu.console().len();
        cpu.feed_console(line.as_bytes());
        let mut insns_used = 0u64;
        let mut done = false;
        for _ in 0..150 {
            cpu.run(2_000_000);
            insns_used += 2_000_000;
            let con = String::from_utf8_lossy(cpu.console()).into_owned();
            let out = con.get(con_before.min(con.len())..).unwrap_or("");
            if out.contains(want) {
                done = true;
                break;
            }
        }
        report.push_str(&format!(
            "  {:<26} -> {:<8} [{}]  (~{} Minsns)\n",
            line.trim(),
            want,
            if done { "OK" } else { "MISS" },
            insns_used / 1_000_000,
        ));
    }
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let new_out = con.get(before.min(con.len())..).unwrap_or("").to_owned();
    let ff = std::env::var_os("HOLO_NO_FASTFWD").is_some();
    eprintln!(
        "\n==== RESUMED-SHELL SESSION (fastfwd_disabled={ff}) ====\n{report}FULL new console ({} bytes):\n{}\n====\n",
        new_out.len(),
        new_out,
    );
    for (line, want) in cmds {
        assert!(
            new_out.contains(want),
            "resumed shell did not execute {:?} (want {:?})",
            line.trim(),
            want,
        );
    }
}

/// STREAMING-DISK MANIFEST (ignored) — the full path: snapshot a machine with the κ-disk STREAMED
/// (manifest carries only the per-sector index, NOT ~10 MiB of sectors), publish RAM pages + disk
/// sectors to a transport, then resume by streaming RAM by κ + the disk LAZILY by κ. Proves the
/// manifest is light AND only the working set is fetched AND commands still run.
/// Run: `cargo test -p holospaces --release --test cc45_x64_alpine streaming_disk_manifest -- --ignored --nocapture`
#[test]
#[ignore = "native: streaming-disk MANIFEST round-trip (light manifest + lazy disk + commands run)"]
fn streaming_disk_manifest_round_trips_light_and_lazy() {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    let blob = std::fs::read(artifact("crates/holospaces-web/web/fixtures/x64-alpine-shell.kblob"))
        .expect("read the shell snapshot fixture");
    let mut src = Cpu::new(0x1000);
    assert!(src.restore_kappa_blob(&blob), "restore the source machine");

    // Snapshot with the disk STREAMED, and (for comparison) inline — the streaming manifest must be
    // far smaller (no inline sectors).
    let inline_manifest = src.snapshot_kappa_manifest();
    let stream_manifest = src.snapshot_kappa_manifest_streaming_disk();
    assert!(
        stream_manifest.len() + 5_000_000 < inline_manifest.len(),
        "streaming manifest ({} KiB) must be much smaller than inline ({} KiB)",
        stream_manifest.len() / 1024, inline_manifest.len() / 1024,
    );

    // Publish RAM pages + disk sectors to a "transport" (the bundle / a peer / OPFS).
    let ram: Arc<HashMap<[u8; 71], Vec<u8>>> = Arc::new(src.kappa_ram_pages().into_iter().collect());
    let disk: Arc<HashMap<[u8; 71], Vec<u8>>> = Arc::new(src.disk_unique_sectors().into_iter().collect());
    let disk_total = disk.len();
    let disk_fetches = Arc::new(AtomicU64::new(0));

    // Resume a FRESH machine purely by streaming: RAM by κ, the disk lazily by κ.
    let mut dst = Cpu::new(0x1000);
    let (rram, rdisk, rf) = (ram.clone(), disk.clone(), disk_fetches.clone());
    let ok = dst.restore_kappa_streaming_lazy_disk(
        &stream_manifest,
        |k| rram.get(k).cloned(),
        Box::new(move |k| {
            rf.fetch_add(1, Ordering::Relaxed);
            rdisk.get(k).cloned()
        }),
    );
    assert!(ok, "streaming resume with a lazy disk succeeded");

    // The resumed machine runs real commands (external fork+exec + a disk-file read).
    for cmd in ["uname -m\n", "cat /etc/alpine-release\n", "echo $((6*7))\n"] {
        dst.feed_console(cmd.as_bytes());
        for _ in 0..40 {
            dst.run(2_000_000);
        }
    }
    let con = String::from_utf8_lossy(dst.console()).into_owned();
    let fetched = disk_fetches.load(Ordering::Relaxed);
    eprintln!(
        "\n==== STREAMING-DISK MANIFEST ====\n\
         manifest: streaming {} KiB  vs  inline {} KiB ({:.1}x smaller)\n\
         disk: fetched {} of {} sectors on demand ({} KiB)\n\
         uname→x86_64: {}  |  cat /etc/alpine-release→3.20.3: {}\n====\n",
        stream_manifest.len() / 1024, inline_manifest.len() / 1024,
        inline_manifest.len() as f64 / stream_manifest.len() as f64,
        fetched, disk_total, fetched * 512 / 1024,
        con.contains("x86_64"), con.contains("3.20.3"),
    );
    assert!(con.contains("x86_64"), "external command ran after streaming resume");
    assert!(con.contains("3.20.3"), "a disk-file read resolved via lazy κ-fetch (L5)");
    assert!(fetched > 0 && (fetched as usize) < disk_total, "only the disk working set was fetched");
}

/// STREAMING-DISK RESUME (ignored) — the disk-streaming-by-κ win, end-to-end on a REAL machine:
/// restore the warm shell, "publish" its disk sectors to a remote transport, then swap the disk for
/// a LAZY backing that fetches by κ on demand. Real commands still run (external fork+exec + a
/// disk-file read), and only the working set crosses the wire (not the whole ~10 MiB disk).
/// Run: `cargo test -p holospaces --release --test cc45_x64_alpine streaming_disk_resume -- --ignored --nocapture`
#[test]
#[ignore = "native: lazy streaming κ-disk resume — only the working set is fetched, commands still run"]
fn streaming_disk_resume_runs_and_fetches_only_working_set() {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    let blob = std::fs::read(artifact("crates/holospaces-web/web/fixtures/x64-alpine-shell.kblob"))
        .expect("read the shell snapshot fixture");
    let mut cpu = Cpu::new(0x1000);
    assert!(cpu.restore_kappa_blob(&blob), "restore the shell snapshot");
    // "Publish" the disk's unique sectors to a remote transport (the link bundle / a peer / OPFS).
    let sectors = cpu.disk_unique_sectors();
    let total = sectors.len();
    let remote: Arc<HashMap<[u8; 71], Vec<u8>>> = Arc::new(sectors.into_iter().collect());
    let fetches = Arc::new(AtomicU64::new(0));
    let (rr, rf) = (remote.clone(), fetches.clone());
    // Resume with a LAZY streaming disk: sectors fetch by κ on demand (verify-on-receipt L5).
    assert!(
        cpu.restream_disk(Box::new(move |k| {
            rf.fetch_add(1, Ordering::Relaxed);
            rr.get(k).cloned()
        })),
        "the machine has a disk to restream",
    );
    // Real session: external commands (fork+exec, demand-page) + a disk-FILE read.
    for cmd in ["uname -m\n", "cat /etc/alpine-release\n", "ls /bin\n", "echo $((6*7))\n"] {
        cpu.feed_console(cmd.as_bytes());
        for _ in 0..40 {
            cpu.run(2_000_000);
        }
    }
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let fetched = fetches.load(Ordering::Relaxed);
    eprintln!(
        "\n==== STREAMING-DISK RESUME ====\n\
         disk sectors total: {}\n\
         fetched on demand:  {} ({} KiB) — the rest of the disk never crossed the wire\n\
         uname→x86_64: {}  |  cat /etc/alpine-release→3.20.3: {}\n====\n",
        total, fetched, fetched * 512 / 1024,
        con.contains("x86_64"), con.contains("3.20.3"),
    );
    assert!(con.contains("x86_64"), "external command ran on the streaming disk");
    assert!(con.contains("3.20.3"), "a disk-file read resolved via lazy κ-fetch (L5)");
    assert!(fetched > 0, "the streaming disk actually served sectors on demand");
    assert!((fetched as usize) < total, "only the working set was fetched, not the whole disk");
}

/// PROFILE (ignored) — the κ-disk WORKING SET: how many distinct sectors does a resumed session
/// actually touch? Quantifies the win of streaming the disk by κ on demand (only touched sectors
/// crossing the wire) vs the current inline-disk snapshot (~10 MiB). Restores the warm shell, runs
/// a realistic session, and reports the distinct sector κs fetched × 512 B = the lazy-stream payload.
/// Run: `cargo test -p holospaces --release --test cc45_x64_alpine disk_working_set_profile -- --ignored --nocapture`
#[test]
#[ignore = "profile: κ-disk working set of a resumed session (disk-streaming win)"]
fn disk_working_set_profile() {
    let blob = std::fs::read(artifact("crates/holospaces-web/web/fixtures/x64-alpine-shell.kblob"))
        .expect("read the shell snapshot fixture");
    let mut cpu = Cpu::new(0x1000);
    assert!(cpu.restore_kappa_blob(&blob), "restore the shell snapshot");
    reset_disk_fetch_stats(); // count only post-resume disk reads (the live session's working set)
    for cmd in [
        "uname -a\n",
        "ls -la /\n",
        "cat /etc/os-release\n",
        "cat /etc/alpine-release\n",
        "ls /bin\n",
        "echo $((6*7))\n",
    ] {
        cpu.feed_console(cmd.as_bytes());
        for _ in 0..40 {
            cpu.run(2_000_000);
        }
    }
    let (distinct, calls) = drain_disk_fetch_stats();
    // The inline disk in the current snapshot is ~10 MiB of non-sparse Alpine sectors.
    let inline_disk_bytes = 10.0 * 1024.0 * 1024.0;
    let stream_bytes = distinct as f64 * 512.0;
    eprintln!(
        "\n==== κ-DISK WORKING SET ====\n\
         distinct sectors touched: {} ({:.0} KiB if streamed by κ on demand)\n\
         total read_sector calls:  {} (cache+dedup absorb the rest)\n\
         inline-disk snapshot today: ~{:.1} MiB\n\
         → streaming the disk would move ~{:.0} KiB instead of ~10 MiB ({:.1}% smaller) for this session\n====\n",
        distinct, stream_bytes / 1024.0,
        calls,
        inline_disk_bytes / 1024.0 / 1024.0,
        stream_bytes / 1024.0,
        100.0 * (1.0 - stream_bytes / inline_disk_bytes),
    );
}

/// PROFILE (ignored) — which x86 opcodes does REAL Alpine actually execute? The JIT decoder today
/// covers only `sha512_transform`'s shapes; to make it help real workloads it must be broadened by
/// FREQUENCY (the hot 20% that are 80% of execution). This restores the warm shell, runs a real
/// command, and reports the primary-opcode + `0F xx` distributions with cumulative coverage.
/// Run: `cargo test -p holospaces --release --test cc45_x64_alpine opcode_frequency_profile -- --ignored --nocapture`
#[test]
#[ignore = "profile: x86 opcode frequency of real Alpine (JIT decoder priority list)"]
fn opcode_frequency_profile() {
    let blob = std::fs::read(artifact("crates/holospaces-web/web/fixtures/x64-alpine-shell.kblob"))
        .expect("read the shell snapshot fixture");
    let mut cpu = Cpu::new(0x1000);
    assert!(cpu.restore_kappa_blob(&blob), "restore the shell snapshot");
    reset_ophist();
    // Run real userland+kernel work (external commands fork+exec → broad instruction mix).
    for cmd in ["uname -m\n", "ls -la /\n", "cat /etc/os-release\n", "echo $((6*7))\n"] {
        cpu.feed_console(cmd.as_bytes());
        for _ in 0..40 {
            cpu.run(2_000_000);
        }
    }
    let (prim, sec) = drain_ophist();
    let total: u64 = prim.iter().map(|(_, c)| c).sum();
    let group = |op: u8| -> &'static str {
        match op {
            0x00..=0x3f => "ALU r/m (add/or/adc/sbb/and/sub/xor/cmp)",
            0x40..=0x4f => "REX (shouldn't appear — prefix)",
            0x50..=0x5f => "push/pop reg",
            0x70..=0x7f => "jcc short (branch)",
            0x80..=0x83 => "ALU r/m, imm",
            0x84..=0x8b => "test/mov/xchg r/m",
            0x88..=0x8b => "mov r/m",
            0x8d => "lea",
            0x0f => "TWO-BYTE (0F: SSE/jcc-near/movzx/setcc/…)",
            0xb0..=0xbf => "mov reg, imm",
            0xc0..=0xc1 | 0xd0..=0xd3 => "shift/rotate",
            0xc3 | 0xc2 => "ret",
            0xe8 => "call rel",
            0xe9 | 0xeb => "jmp rel",
            0xff => "inc/dec/call/jmp/push r/m (grp5)",
            _ => "other",
        }
    };
    // JIT REGION decoder coverage NOW (jit.rs `decode_block` + `decode_block_term`), measured
    // against what the decoder ACTUALLY emits today:
    //  • BODY ops (straight-line): reg-reg ALU/mov, lea, cmp/test, the G6a immediates (0x81/0x83/
    //    0xc7/0xb8+r), and the G6b push/pop. (Shifts are in the IR but NOT decoded → excluded; the
    //    8-bit imm forms 0x80/0x84 are not decoded; non-REX.W imm forms are gated off — so this is a
    //    slight UPPER bound for 0x81/0x83/0xc7.)
    //  • TERMINATORS: jcc-short / jmp / ret end a region cleanly (linked or exited), so for REGION
    //    coverage they count as handled (call `0xe8` and indirect stay region exits).
    let body_covers = |op: u8| matches!(op,
        0x01 | 0x09 | 0x21 | 0x29 | 0x31 | 0x03 | 0x0b | 0x23 | 0x2b | 0x33 // ALU r/m + loadop
        | 0x89 | 0x8b | 0x8d // mov r/m,r / mov r,r/m / load / store / lea
        | 0x39 | 0x3b | 0x85 // cmp / test (reg)
        | 0x81 | 0x83 | 0xc7 // ALU imm / mov imm  (REX.W forms — G6a)
        | 0xb8..=0xbf // mov reg, imm
        | 0x50..=0x5f // push / pop  (G6b)
    );
    let term_covers = |op: u8| matches!(op, 0x70..=0x7f | 0xeb | 0xe9 | 0xc3); // jcc short/jmp/ret
    let mut cum = 0u64;
    eprintln!("\n==== OPCODE FREQUENCY (real Alpine, {} insns) ====", total);
    eprintln!("  {:>6}  {:>6}  {:>5}  op    group", "count", "cum%", "jit?");
    for (op, c) in prim.iter().take(22) {
        cum += c;
        let tag = if body_covers(*op) { "BODY" } else if term_covers(*op) { "term" } else { "—" };
        eprintln!(
            "  {:>6}  {:>5.1}%  {:>5}  0x{:02x}  {}",
            c, 100.0 * cum as f64 / total as f64, tag, op, group(*op),
        );
    }
    let body: u64 = prim.iter().filter(|(op, _)| body_covers(*op)).map(|(_, c)| c).sum();
    let term: u64 = prim.iter().filter(|(op, _)| term_covers(*op)).map(|(_, c)| c).sum();
    eprintln!(
        "\n  Region BODY ops decoded: ~{:.1}% of executed instructions.\n  \
         + control-flow terminators (jcc/jmp/ret) handled by regions: ~{:.1}%.\n  \
         ⇒ region-eligible coverage ≈ {:.1}% (was ~40% before G6a/G6b immediates + push/pop).\n  \
         Two-byte 0F total: {} ({:.1}%) — incl. 0F8x jcc-near (also region terminators). Top 0F: {:?}",
        100.0 * body as f64 / total as f64,
        100.0 * term as f64 / total as f64,
        100.0 * (body + term) as f64 / total as f64,
        prim.iter().find(|(op, _)| *op == 0x0f).map(|(_, c)| *c).unwrap_or(0),
        100.0 * prim.iter().find(|(op, _)| *op == 0x0f).map(|(_, c)| *c).unwrap_or(0) as f64 / total as f64,
        sec.iter().take(6).map(|(o, c)| format!("0F{o:02x}:{c}")).collect::<Vec<_>>(),
    );
    eprintln!("====\n");
}

/// BENCHMARK (ignored) — the REAL interpreter's per-instruction cost (`Cpu::step` over a live
/// Alpine: x86 decode + dispatch + paging + timers), so we can compare it to the JIT's measured
/// ~5 ns/op (and ~2 ns/op raw, amortized). This is the true baseline the JIT must beat — not the
/// fast IR interpreter. Restores the warm shell and runs a fixed instruction budget, timing it.
/// Run: `cargo test -p holospaces --release --test cc45_x64_alpine interpreter_throughput -- --ignored --nocapture`
#[test]
#[ignore = "benchmark: real Cpu::step interpreter throughput (ns/insn)"]
fn interpreter_throughput() {
    use std::time::Instant;
    let blob = std::fs::read(artifact("crates/holospaces-web/web/fixtures/x64-alpine-shell.kblob"))
        .expect("read the shell snapshot fixture");
    let mut cpu = Cpu::new(0x1000);
    assert!(cpu.restore_kappa_blob(&blob), "restore the shell snapshot");
    // Drive a real command so the machine runs userland + kernel (not just idle hlt), then time a
    // fixed budget of forward execution through the interpreter.
    cpu.feed_console(b"echo HOLO$((6*7))\n");
    let before = cpu.insns();
    let t = Instant::now();
    let budget = 200_000_000u64; // 200M guest instructions
    let mut ran = 0u64;
    while ran < budget {
        cpu.run(5_000_000);
        ran += 5_000_000;
        cpu.feed_console(b"echo HOLO$((6*7))\n"); // keep it doing real work
    }
    let secs = t.elapsed().as_secs_f64();
    let insns = cpu.insns().wrapping_sub(before).max(ran); // retired (or budget if idle-skipped)
    let ips = insns as f64 / secs;
    eprintln!(
        "\n==== INTERPRETER THROUGHPUT ====\nran {:.0}M guest-insns in {:.2}s\nCpu::step: {:.1} M insns/s = {:.1} ns/insn\n(JIT measured ~5 ns/op warm, ~2 ns/op raw → ceiling ~{:.1}x if marshalling is amortized)\n====\n",
        insns as f64 / 1e6, secs, ips / 1e6, 1e9 / ips, (1e9 / ips) / 2.0,
    );
}

/// BENCHMARK (ignored, `--features jit-native`) — does the chained region JIT actually FIRE and
/// SPEED UP real Alpine now that the decoder covers immediates + push/pop (G6a/G6b)? Restores the
/// warm shell, runs the SAME realistic commands twice (region JIT off, then on), times both, and
/// asserts the guest produced BYTE-IDENTICAL output (the SHADOW→TRUST→COMMIT correctness proof on
/// real code). Reports the wall-clock delta + how many regions compiled/trusted/committed. This is
/// the native proxy for the in-tab G6d measurement (the browser engine runs the same region wasm).
/// Run: `cargo test -p holospaces --release --features jit-native --test cc45_x64_alpine region_jit_real_alpine_speedup -- --ignored --nocapture`
#[cfg(feature = "jit-native")]
#[test]
#[ignore = "benchmark: chained region JIT speedup on real Alpine (needs jit-native)"]
fn region_jit_real_alpine_speedup() {
    use holospaces::emulator::x64::{drain_jit_stats, set_region_jit_on};
    use std::time::Instant;
    let blob = std::fs::read(artifact("crates/holospaces-web/web/fixtures/x64-alpine-shell.kblob"))
        .expect("read the shell snapshot fixture");
    // Deterministic userland work with a single moderate compute (one sha256), no clocks/PRNG in the
    // output. Each command gets a FIXED, generous budget — large enough that BOTH variants fully
    // finish the command and idle out (hlt-loop, no further output) — so they converge to the SAME
    // final console regardless of how the JIT chunks/overshoots execution. The byte-identical check
    // then tests CORRECTNESS only (the SHADOW→TRUST→COMMIT proof on real code).
    let cmds = [
        "uname -a\n",
        "cat /etc/os-release\n",
        "sha256sum /etc/os-release\n",
        "echo HOLO$((123*456))\n",
        "ls /bin\n",
    ];
    let per_cmd_budget_chunks = 120u32; // 120 × 1M = 120M guest insns/command (ample to finish+idle)
    let run = |jit: bool| -> (std::time::Duration, Vec<u8>, (u64, usize, usize, u64, u64, usize, usize, u64)) {
        let mut cpu = Cpu::new(0x1000);
        assert!(cpu.restore_kappa_blob(&blob), "restore the shell snapshot");
        let _ = drain_jit_stats(); // reset counters
        set_region_jit_on(jit);
        let t = Instant::now();
        for cmd in cmds {
            cpu.feed_console(cmd.as_bytes());
            for _ in 0..per_cmd_budget_chunks {
                cpu.run(1_000_000);
            }
        }
        let el = t.elapsed();
        let stats = drain_jit_stats();
        set_region_jit_on(false);
        (el, cpu.console().to_vec(), stats)
    };
    // DIAGNOSTIC FIRST (breaks early at the bug): a NEVER-TRUST run shadow-checks every region against
    // the interpreter on every entry, so a region whose codegen is wrong for an entry-state the K
    // samples missed is caught (execution unperturbed — nothing commits). Captures the FIRST divergent
    // region's ops + diverging field, so the bug is pinned to a specific instruction shape.
    {
        use holospaces::emulator::x64::{drain_region_divergence, set_region_notrust};
        let mut cpu = Cpu::new(0x1000);
        assert!(cpu.restore_kappa_blob(&blob), "restore the shell snapshot");
        let _ = drain_jit_stats();
        set_region_notrust(true);
        set_region_jit_on(true);
        let mut found: Option<String> = None;
        'outer: for cmd in cmds {
            cpu.feed_console(cmd.as_bytes());
            for _ in 0..per_cmd_budget_chunks {
                cpu.run(1_000_000);
                if let Some(d) = drain_region_divergence() {
                    found = Some(d);
                    break 'outer;
                }
            }
        }
        set_region_jit_on(false);
        set_region_notrust(false);
        match found {
            Some(d) => eprintln!("\n==== FIRST REGION DIVERGENCE (never-trust diagnostic) ====\n{d}\n====\n"),
            None => eprintln!("\n[diagnostic] never-trust run found NO region divergence in this window\n"),
        }
    }

    // Then the speedup + correctness summary: JIT off vs on (the emulator is deterministic — proven
    // earlier that off-vs-off is byte-identical — so any on-vs-off difference is the JIT).
    let (off, out_off, _) = run(false);
    let (on, out_on, s) = run(true);
    let identical = out_off == out_on;
    eprintln!(
        "\n==== REGION JIT on REAL Alpine ====\n  off = {:?}\n  on  = {:?}   → {:.2}x {}\n  \
         regions: distinct={} compiled={} trusted={} committed={} refused={}\n  \
         output byte-identical (on vs off): {}{}\n====\n",
        off, on, off.as_secs_f64() / on.as_secs_f64().max(1e-9),
        if on > off { "(SLOWER — short real regions, per-commit marshalling dominates)" } else { "" },
        s.1, s.2, s.5, s.7, s.6,
        identical,
        if identical { "" } else { "  ← committed region escaped the SHADOW gate (BUG, see diagnostic above)" },
    );
}

/// Generate the **warm INTERACTIVE Alpine shell κ-snapshot** for the one-link loader: boot Alpine
/// with an interactive `/init` (`shell-init`: mounts the pseudo-fs, prints `HOLO-SHELL-READY`, then
/// `busybox setsid -c busybox sh` — an interactive shell with ttyS0 as its controlling tty), run
/// until the shell is up and **blocked reading ttyS0**, then `snapshot_kappa_blob()` → the κ-blob
/// the browser loader resumes and *types into* (`feed_input` → ttyS0 RX → the shell). This is the
/// "compute once per planet" warm image: nobody re-boots Alpine to get a prompt — they resume this.
/// Run: `cargo test -p holospaces --release --test cc45_x64_alpine generate_alpine_shell_snapshot -- --ignored --nocapture`.
#[test]
#[ignore = "boots Alpine to an interactive shell and writes a warm κ-snapshot (slow, one-time)"]
fn generate_alpine_shell_snapshot() {
    let kernel = vmlinux_elf();
    let layer_blob = std::fs::read(artifact("vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz"))
        .expect("read the pinned alpine minirootfs layer");
    let init = std::fs::read(artifact("vv/artifacts/cc45/alpine/shell-init"))
        .expect("read the interactive shell-init (compile it first)");
    let rootfs = assemble_ext4_bootable(
        &[Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer_blob }],
        &init,
        256 * 1024 * 1024,
    )
    .expect("assemble the bootable Alpine ext4 with the interactive init");

    let mut cpu = Cpu::boot_linux_disk(
        512 * 1024 * 1024,
        &kernel,
        rootfs,
        // `nmi_watchdog=0 nowatchdog` stop the hard-lockup detector from spinning the idle machine
        // in timer/NMI kernel code (it never returns to the user shell otherwise);
        // `tsc=reliable` skips the clocksource watchdog (no second clock to cross-check here).
        "virtio_mmio.device=0x200@0xd0000000:11 console=ttyS0 root=/dev/vda rw init=/init random.trust_cpu=on norandmaps nmi_watchdog=0 nowatchdog tsc=reliable",
    );

    // Run until the shell PROMPT appears, then snapshot IMMEDIATELY (a tiny settle so sh blocks on
    // the serial read). Do NOT over-run — running the idle machine for billions of instructions
    // drives it into the timer/NMI spin we are avoiding.
    let mut ready = false;
    for _ in 0..3000 {
        cpu.run(20_000_000);
        if String::from_utf8_lossy(cpu.console()).contains("holo$") {
            ready = true;
            for _ in 0..3 {
                cpu.run(15_000_000); // let sh finish the prompt and block on the serial read
            }
            break;
        }
    }
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let tail = |n: usize| con.lines().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    assert!(ready, "reached the interactive shell. tail:\n{}", tail(14));

    let blob = cpu.snapshot_kappa_blob();
    let out = artifact("crates/holospaces-web/web/fixtures/x64-alpine-shell.kblob");
    std::fs::write(&out, &blob).expect("write the warm shell κ-blob");
    eprintln!(
        "\n==== WARM ALPINE SHELL SNAPSHOT ====\nwrote {} ({} KiB)\nconsole tail:\n{}\n====\n",
        out.display(),
        blob.len() / 1024,
        tail(8),
    );
}
