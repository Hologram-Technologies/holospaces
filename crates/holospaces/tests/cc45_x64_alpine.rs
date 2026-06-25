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
use holospaces::emulator::x64::{Cpu, Halt};

fn artifact(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

/// The CC-44 platform kernel, gunzipped — the x86-64 core enters `startup_64`
/// directly (64-bit boot protocol, no in-guest decompressor).
fn vmlinux_elf() -> Vec<u8> {
    let gz = artifact("vv/artifacts/cc44/linux/vmlinux.gz");
    let mut img = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(&gz).expect("read cc44 vmlinux.gz")[..])
        .read_to_end(&mut img)
        .expect("gunzip the kernel ELF");
    img
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
        "console=ttyS0 root=/dev/vda rw init=/init random.trust_cpu=on",
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
