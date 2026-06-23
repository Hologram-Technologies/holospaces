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
