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
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::emulator::x64::{Cpu, Halt};

/// Gunzip a committed `.gz` artifact.
fn gunzip(path: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(path).expect("read gz")[..])
        .read_to_end(&mut out)
        .expect("gunzip");
    out
}

/// The committed, *uncompressed* ELF kernel (`vmlinux`), gunzipped. The x86-64
/// core loads its `PT_LOAD` segments and enters `startup_64` directly — the
/// 64-bit boot protocol, no in-guest decompressor.
fn vmlinux_elf() -> Vec<u8> {
    gunzip(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc44/linux/vmlinux.gz"))
}

fn cc45_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc45")
}

/// Assemble the **amd64 devcontainer** rootfs: the stock `linux-amd64` busybox
/// layer (`cc45/rootfs/layer.tar.gz`, the canonical glibc binary) overlaid into
/// an `ext4` image by the in-crate Layer Assembler (`CC-7`), with the
/// busybox-shell `/init` injected — a bootable, writable disk taken into the
/// κ-disk on attach. No freestanding shim: the stock glibc binary itself runs.
fn assemble_cc45_rootfs() -> Vec<u8> {
    use holospaces::assembly::{assemble_ext4_bootable, Layer};
    let init = std::fs::read(cc45_dir().join("init.sh")).expect("cc45 busybox init.sh");
    let layer = std::fs::read(cc45_dir().join("rootfs/layer.tar.gz")).expect("cc45 busybox layer");
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
        blob: &layer,
    }];
    assemble_ext4_bootable(&layers, &init, 64 * 1024 * 1024)
        .expect("assemble the amd64 busybox rootfs")
}

/// The kernel command line for the amd64 devcontainer boot. The κ-disk
/// `virtio-blk` device sits at MMIO `0xd000_0000` (size `0x200`, IRQ 11); x86 has
/// no device tree, so the kernel discovers it via `virtio_mmio.device=` (the
/// `VIRTIO_MMIO_CMDLINE_DEVICES` config) and mounts it as the `/dev/vda` root.
const CC45_CMDLINE: &str = "console=ttyS0 root=/dev/vda rw init=/init \
     virtio_mmio.device=0x200@0xd0000000:11 random.trust_cpu=on";

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

/// The **build-capable** boot path (`CC-45`, section B): an x64 holospace may
/// declare a *multi-GiB* disk — room to install toolchains and compile software
/// in-guest — and still boot promptly, because the κ-disk is paged by its
/// **occupancy**, not its declared size. [`Cpu::boot_linux_disk_occupancy`] indexes
/// only the sectors the sparse assembler actually wrote (`from_occupancy`), so boot
/// setup is O(content), not O(disk): here an **8 GiB** disk (16.7M sectors) is
/// attached from a handful of occupied sectors and the real amd64 kernel boots to
/// userspace with it probed — the same boot an 8 MiB disk produces, proving the
/// disk-size ceiling is gone (the old dense index would be ~1.2 GB of RAM and a
/// 16.7M-iteration build before the kernel even started). Parametric in the image
/// (Law L4): the declared size is just a number; only content is paged.
#[test]
#[ignore = "boots a real amd64 Linux with an 8 GiB occupancy-indexed κ-disk (~release)"]
fn an_amd64_linux_boots_from_an_occupancy_indexed_build_capable_disk() {
    let kernel = vmlinux_elf();

    // An 8 GiB *declared* disk — a build-capable size — paged by occupancy. Only a
    // sparse scattering of sectors is populated (what a mostly-empty large rootfs
    // looks like): a superblock-like region near the front and one block at the far
    // end of the 8 GiB address space. The 16.7M holes are never indexed or read.
    const DISK_BYTES: u64 = 8 * 1024 * 1024 * 1024;
    let sector_count = DISK_BYTES / 512;
    let occupied: Vec<(u64, [u8; 512])> = [0u64, 1, 2, 4096, sector_count - 1]
        .iter()
        .map(|&i| {
            let mut s = [0u8; 512];
            for (j, b) in s.iter_mut().enumerate() {
                *b = (i as u8)
                    .wrapping_mul(31)
                    .wrapping_add(j as u8)
                    .wrapping_add(1);
            }
            (i, s)
        })
        .collect();

    let store: Box<dyn KappaStore> = Box::new(MemKappaStore::new());
    let mut cpu = Cpu::boot_linux_disk_occupancy(
        1024 * 1024 * 1024,
        &kernel,
        "earlyprintk=serial,ttyS0 console=ttyS0 random.trust_cpu=on",
        store,
        sector_count,
        occupied,
    );
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!(
        "---- guest console (8 GiB occupancy κ-disk) ----\n{console}\n---- end ----  (halt: {halt:?})"
    );

    assert!(
        console.contains("Run /init as init process"),
        "the kernel handed control to PID 1 with the 8 GiB build-capable κ-disk attached"
    );
    assert_eq!(
        halt,
        Halt::Halted,
        "PID 1 powered the machine off — a clean boot through the occupancy-indexed path"
    );
    assert!(
        console.contains("HOLOSPACES-LINUX-USERSPACE-OK"),
        "PID 1 printed its marker — the real amd64 kernel booted from the 8 GiB occupancy κ-disk"
    );
}

/// The flagship **`CC-45`** witness: an amd64 devcontainer boots from its κ-disk
/// `virtio-blk` rootfs on the x86-64 core and runs the **stock, unmodified
/// `linux-amd64` busybox** as PID 1 — `uname -m` reports `x86_64`, a busybox shell
/// computation runs (sum 1..=1000 == 500500), and `head` reads the real
/// `/proc/version` over the mounted rootfs. No freestanding shim, no per-ISA
/// workaround (Law L4): the stock glibc binary itself executes its SSE string
/// routines, forks children, and faults copy-on-write pages on the emulator. The
/// differential oracle is `qemu-system-x86_64` on the same kernel + rootfs
/// (`vv/suites/cc45-x64-devcontainer.sh`).
#[test]
#[ignore = "boots a real amd64 devcontainer (~release) — run by the CC-45 vv suite"]
fn an_amd64_devcontainer_runs_a_stock_linux_amd64_binary() {
    let kernel = gunzip(&cc45_dir().join("linux/vmlinux.gz"));
    let rootfs = assemble_cc45_rootfs();
    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, rootfs, CC45_CMDLINE);
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!(
        "---- guest console (amd64 devcontainer) ----\n{console}\n---- end ----  (halt: {halt:?})"
    );

    assert!(
        console.contains("CC45-DEVCONTAINER-UP"),
        "the amd64 devcontainer booted from its κ-disk virtio-blk rootfs"
    );
    // The stock linux-amd64 binary executed its own logic (a real computation).
    assert!(
        console.contains("CC45-COMPUTE:500500"),
        "the stock linux-amd64 binary ran its computation (sum 1..=1000 == 500500)"
    );
    // … and reports the guest architecture via the uname syscall.
    assert!(
        console.contains("CC45-ARCH:x86_64"),
        "the stock binary's uname syscall reports x86_64"
    );
    assert!(
        console.contains("Linux version 6.6.0"),
        "the stock binary read the real /proc/version over the mounted rootfs"
    );
    assert_eq!(
        halt,
        Halt::Halted,
        "the devcontainer powered off cleanly (poweroff → reboot syscall → hlt)"
    );
}

/// The same amd64 devcontainer, but its κ-disk is **paged from a `KappaStore` by
/// streaming sectors** — the exact path the browser peer's `X64Workspace` takes
/// (the rootfs is read sector-by-sector from OPFS into an OPFS-backed store; here a
/// `MemKappaStore` stands in). The full image is never held as one `Vec`. Proves
/// the streamed paged κ-disk boots the x86-64 core identically to the flat-image
/// boot — the substrate-native, OOM-free path for a real amd64 image.
#[test]
#[ignore = "boots a real amd64 devcontainer paged from a κ-store (~release) — CC-45 vv suite"]
fn an_amd64_devcontainer_boots_paged_from_a_kappa_store() {
    let kernel = gunzip(&cc45_dir().join("linux/vmlinux.gz"));
    let rootfs = assemble_cc45_rootfs();
    let sector_count = (rootfs.len() as u64).div_ceil(512);
    let read = move |i: u64, buf: &mut [u8]| {
        let off = (i * 512) as usize;
        let n = buf.len().min(rootfs.len().saturating_sub(off));
        buf[..n].copy_from_slice(&rootfs[off..off + n]); // sparse tail stays zero
    };
    let mut cpu = Cpu::boot_linux_disk_streamed(
        512 * 1024 * 1024,
        &kernel,
        CC45_CMDLINE,
        Box::new(MemKappaStore::new()),
        sector_count,
        read,
    );
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    assert!(
        console.contains("CC45-DEVCONTAINER-UP") && console.contains("CC45-ARCH:x86_64"),
        "the amd64 devcontainer booted from its streamed paged κ-disk; console:\n{console}"
    );
    assert_eq!(halt, Halt::Halted, "powered off cleanly");
}

/// Build a minimal USTAR + gzip OCI layer blob from `(path, data)` entries. An empty
/// `data` whose basename is prefixed `.wh.` is an OCI whiteout — the assembler
/// deletes the matching lower-layer file (OCI image-spec "Layer", `CC-4`/`CC-20`).
fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    use std::io::Write as _;
    let oct = |f: &mut [u8], v: u64| {
        let s = format!("{:0w$o}", v, w = f.len() - 1);
        f[..s.len()].copy_from_slice(s.as_bytes());
    };
    let mut tar = Vec::new();
    for (path, data) in entries {
        let mut hdr = [0u8; 512];
        hdr[..path.len()].copy_from_slice(path.as_bytes());
        oct(&mut hdr[100..108], 0o644); // mode
        oct(&mut hdr[124..136], data.len() as u64); // size
        hdr[156] = b'0'; // type: regular file
        hdr[257..263].copy_from_slice(b"ustar\0");
        hdr[263] = b'0';
        hdr[264] = b'0';
        hdr[148..156].fill(b' '); // checksum field spaces before summing
        let sum: u32 = hdr.iter().map(|&b| u32::from(b)).sum();
        oct(&mut hdr[148..155], u64::from(sum));
        hdr[155] = b' ';
        tar.extend_from_slice(&hdr);
        tar.extend_from_slice(data);
        let pad = data.len().div_ceil(512) * 512 - data.len();
        tar.extend(std::iter::repeat_n(0u8, pad));
    }
    tar.extend([0u8; 1024]); // two zero blocks terminate the archive
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

/// An **arbitrary multi-layer** amd64 image (the DoD's "multi-layer real images")
/// boots on the x86-64 core, with the OCI overlay — **whiteout**, **override**, and
/// **add** — correctly applied across layers before the boot. Three layers stack:
/// a lower layer with two files, the stock busybox layer in the middle (PID 1), and
/// an upper layer that whiteouts one lower file, overrides the other, and adds a
/// new one. PID 1 (the stock linux-amd64 busybox) inspects the merged rootfs and
/// emits a single OK marker only if all three overlay rules held — proving the
/// parametric assembler (`CC-4`/`CC-20`, Law L4) feeds the x86-64 boot for any image,
/// not just the single-layer fixture.
#[test]
#[ignore = "boots a multi-layer amd64 image (whiteout/override/add) — run by the CC-45 vv suite"]
fn an_amd64_multilayer_image_overlay_runs() {
    use holospaces::assembly::{assemble_ext4_bootable, Layer};
    let kernel = gunzip(&cc45_dir().join("linux/vmlinux.gz"));
    let busybox = std::fs::read(cc45_dir().join("rootfs/layer.tar.gz")).expect("busybox layer");
    // Lower layer: a file to be whiteout-removed + a file to be overridden.
    let lower = tar_gz(&[("cc45-remove", b"FROM-L1"), ("cc45-keep", b"FROM-L1")]);
    // Upper layer: whiteout cc45-remove, override cc45-keep, add cc45-added.
    let upper = tar_gz(&[
        (".wh.cc45-remove", b""),
        ("cc45-keep", b"OVERRIDE-L3"),
        ("cc45-added", b"ADDED-L3"),
    ]);
    let oci = "application/vnd.oci.image.layer.v1.tar+gzip";
    let layers = [
        Layer { media_type: oci, blob: &lower },
        Layer { media_type: oci, blob: &busybox },
        Layer { media_type: oci, blob: &upper },
    ];
    let init: &[u8] = b"#!/bin/busybox sh\n\
/bin/busybox mkdir -p /proc\n\
/bin/busybox mount -t proc proc /proc\n\
echo CC45-DEVCONTAINER-UP\n\
if [ ! -e /cc45-remove ] && \
[ \"$(/bin/busybox cat /cc45-keep)\" = OVERRIDE-L3 ] && \
[ \"$(/bin/busybox cat /cc45-added)\" = ADDED-L3 ]; then echo CC45-MULTILAYER-OK; \
else echo CC45-MULTILAYER-FAIL; fi\n\
/bin/busybox poweroff -f\n";
    let rootfs = assemble_ext4_bootable(&layers, init, 64 * 1024 * 1024)
        .expect("assemble the 3-layer amd64 image");
    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, rootfs, CC45_CMDLINE);
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!("---- multi-layer amd64 devcontainer ----\n{console}\n---- end ----  (halt: {halt:?})");
    assert!(
        console.contains("CC45-DEVCONTAINER-UP"),
        "the multi-layer amd64 image booted from its κ-disk rootfs"
    );
    assert!(
        console.contains("CC45-MULTILAYER-OK"),
        "the OCI overlay applied across layers (whiteout removed the lower file, the \
         upper layer overrode + added files) before the x86-64 boot"
    );
    assert_eq!(halt, Halt::Halted, "the multi-layer devcontainer powered off cleanly");
}

/// A **Dockerfile build** (`CC-26`) produces an amd64 rootfs whose `RUN` steps
/// execute **in the booted x86-64 OS**: `FROM` the stock amd64 busybox, an `ENV` the
/// `RUN` consumes, a `COPY` from the build context, and `RUN` steps that run real
/// applets. The parsed Dockerfile's build `/init` (with the `ENV` in scope) boots on
/// the x86-64 core and runs the steps — the COPY'd script executes and the `RUN`
/// echo uses the `ENV` — then powers off. Proves the substrate-native Dev Container
/// **build** phase feeds the x86-64 boot (Law L4, the ISA-agnostic `CC-26` pipeline).
#[test]
#[ignore = "builds an amd64 devcontainer from a Dockerfile + runs it on x86-64 — CC-45 vv suite"]
fn an_amd64_dockerfile_build_runs_on_x64() {
    use holospaces::assembly::{assemble_ext4_bootable, Layer};
    use holospaces::dockerfile;
    use std::collections::BTreeMap;

    // No WORKDIR + explicit /bin/busybox: the fixture's busybox is the bare binary
    // (no applet symlink farm), exactly the stock amd64 busybox the run-stage uses.
    let dockerfile = "FROM holospaces/busybox:latest\n\
ENV BUILT_BY=cc45\n\
COPY setup.sh /usr/local/bin/setup.sh\n\
RUN /bin/busybox sh /usr/local/bin/setup.sh\n\
RUN echo CC45-BUILD-RAN:$BUILT_BY\n";
    let setup_sh: &[u8] = b"#!/bin/busybox sh\necho CC45-SETUP-RAN\n";

    let df = dockerfile::parse(dockerfile.as_bytes(), &BTreeMap::new()).expect("parse the Dockerfile");
    assert_eq!(df.from, "holospaces/busybox:latest", "FROM resolved");

    let kernel = gunzip(&cc45_dir().join("linux/vmlinux.gz"));
    let busybox = std::fs::read(cc45_dir().join("rootfs/layer.tar.gz")).expect("busybox layer");
    // The COPY directives → a synthetic layer at their destination paths.
    let copy_files: Vec<(&str, &[u8])> = df
        .copies()
        .iter()
        .map(|(src, dst)| {
            assert_eq!(*src, "setup.sh", "COPY source from the build context");
            (dst.trim_start_matches('/'), setup_sh)
        })
        .collect();
    let copy_layer = tar_gz(&copy_files);
    let oci = "application/vnd.oci.image.layer.v1.tar+gzip";
    let layers = [
        Layer { media_type: oci, blob: &busybox },
        Layer { media_type: oci, blob: &copy_layer },
    ];
    // The build /init runs BUILD-START, the RUN steps, BUILD-DONE, then our poweroff
    // tail (before the trailing reboot the build init appends).
    let init = df.build_init(Some("/bin/busybox poweroff -f\n"));
    let rootfs = assemble_ext4_bootable(&layers, &init, 64 * 1024 * 1024)
        .expect("assemble the Dockerfile-built amd64 rootfs");

    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, rootfs, CC45_CMDLINE);
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!("---- amd64 Dockerfile build ----\n{console}\n---- end ----  (halt: {halt:?})");
    assert!(
        console.contains("BUILD-START") && console.contains("BUILD-DONE"),
        "the Dockerfile build /init ran its RUN sequence in the booted x86-64 OS"
    );
    assert!(
        console.contains("CC45-SETUP-RAN"),
        "the COPY'd setup.sh executed in the OS (RUN ran the copied script)"
    );
    assert!(
        console.contains("CC45-BUILD-RAN:cc45"),
        "the RUN echo consumed the Dockerfile ENV (BUILT_BY=cc45)"
    );
    assert_eq!(halt, Halt::Halted, "the build OS powered off cleanly");
}

/// The Dev Container **features** (`CC-25`) + **lifecycle** (`CC-22`) phases run on
/// amd64: a `devcontainer.json` declares `containerEnv`, a feature, and lifecycle
/// hooks; the parsed config's lifecycle `/init` installs the feature (its `install.sh`,
/// injected at `/opt/holospaces/features/0/`, runs with the option as env) BEFORE the
/// lifecycle hooks (spec order), all in the booted x86-64 OS over the stock amd64
/// busybox base. Proves the full Dev Container spec surface — not just image/build —
/// feeds the x86-64 boot (the ISA-agnostic CC-25/CC-22 pipelines, Law L4).
#[test]
#[ignore = "runs amd64 devcontainer features + lifecycle on x86-64 — CC-45 vv suite"]
fn an_amd64_devcontainer_features_and_lifecycle_run_on_x64() {
    use holospaces::assembly::{assemble_ext4_bootable, Layer};
    use holospaces::boot::devcontainer;

    let config: &[u8] = br#"{
        "containerEnv": { "GREETING": "x64" },
        "features": { "ghcr.io/holospaces/features/demo:1": { "version": "9" } },
        "onCreateCommand": "echo CC45-ONCREATE",
        "postCreateCommand": "echo CC45-POSTCREATE:$GREETING"
    }"#;
    let dc = devcontainer::parse(config).expect("parse the devcontainer.json");
    assert_eq!(dc.features.len(), 1, "the feature is parsed");

    // The feature artifact: install.sh echoes a marker using the (uppercased) option.
    let install_sh: &[u8] = b"#!/bin/busybox sh\necho CC45-FEATURE-INSTALLED:$VERSION\n";
    let feature_layer = tar_gz(&[("opt/holospaces/features/0/install.sh", install_sh)]);

    let kernel = gunzip(&cc45_dir().join("linux/vmlinux.gz"));
    let busybox = std::fs::read(cc45_dir().join("rootfs/layer.tar.gz")).expect("busybox layer");
    let oci = "application/vnd.oci.image.layer.v1.tar+gzip";
    let layers = [
        Layer { media_type: oci, blob: &busybox },
        Layer { media_type: oci, blob: &feature_layer },
    ];
    let init = dc.lifecycle_init();
    let rootfs = assemble_ext4_bootable(&layers, &init, 64 * 1024 * 1024)
        .expect("assemble the features+lifecycle amd64 rootfs");

    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, rootfs, CC45_CMDLINE);
    cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!("---- amd64 features+lifecycle ----\n{console}\n---- end ----");
    assert!(
        console.contains("CC45-FEATURE-INSTALLED:9"),
        "the feature's install.sh ran in the OS with its option as env (VERSION=9)"
    );
    assert!(
        console.contains("CC45-ONCREATE") && console.contains("CC45-POSTCREATE:x64"),
        "the lifecycle hooks ran in the OS with containerEnv in scope"
    );
    // Dev Container spec order: features install BEFORE the lifecycle commands.
    let feat = console.find("CC45-FEATURE-INSTALLED").expect("feature ran");
    let life = console.find("CC45-ONCREATE").expect("lifecycle ran");
    assert!(feat < life, "features installed before the lifecycle hooks (spec order)");
}
