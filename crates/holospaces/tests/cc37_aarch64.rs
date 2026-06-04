//! `CC-37` — an arm64 devcontainer runs the ecosystem's stock `linux-arm64`
//! binaries (ADR-021, arc42 ch.10).
//!
//! The implementation under test is the AArch64 system
//! ([`holospaces::emulator::aarch64`]) booting from a **κ-disk `virtio-blk`
//! rootfs** — the same substrate-backed `virtio` device the RISC-V machine boots
//! (the shared [`emulator::devbus`], no per-ISA re-implementation). The
//! authorities are the Dev Container + OCI image specs over an `arm64/linux`
//! image, and a **stock, unmodified `linux-arm64` busybox** binary
//! (`vv/artifacts/cc37/rootfs/`, built by the upstream busybox + the
//! `aarch64-linux-gnu` toolchain) as the witness that arbitrary `linux-arm64`
//! binaries run with no riscv64 workaround. The rootfs is assembled into the
//! κ-disk by the in-crate Layer Assembler (`CC-7`); the differential oracle is
//! `qemu-system-aarch64 -M virt` on the same kernel + rootfs.

use std::io::Read;
use std::path::{Path, PathBuf};

use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::aarch64::{Cpu, Halt};

fn cc37_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc37")
}

fn gunzip(path: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(path).expect("read gz")[..])
        .read_to_end(&mut out)
        .expect("gunzip");
    out
}

/// Assemble the arm64 devcontainer rootfs: the **stock `linux-arm64` busybox**
/// layer (`rootfs/layer.tar.gz`, the canonical glibc binary — Advanced-SIMD
/// ifunc string routines and all) overlaid into an `ext4` image, with the
/// busybox-shell `/init` injected — a bootable, writable disk taken into the
/// κ-disk on attach. No freestanding shim: the stock glibc binary itself runs.
fn assemble_rootfs() -> Vec<u8> {
    let init = std::fs::read(cc37_dir().join("init.sh")).expect("cc37 busybox init.sh");
    let layer = std::fs::read(cc37_dir().join("rootfs/layer.tar.gz")).expect("cc37 busybox layer");
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
        blob: &layer,
    }];
    assemble_ext4_bootable(&layers, &init, 64 * 1024 * 1024)
        .expect("assemble the arm64 busybox rootfs")
}

/// The flagship `CC-37` witness: an arm64 devcontainer boots from its κ-disk
/// `virtio-blk` rootfs and runs the stock `linux-arm64` busybox — `uname -m`
/// reports `aarch64`, the real `/proc/version` is read, and a busybox
/// computation runs — all unmodified, over the shared virtio device.
#[test]
#[ignore = "boots a real arm64 devcontainer (~release) — run by the CC-37 vv suite"]
fn an_arm64_devcontainer_runs_a_stock_linux_arm64_binary() {
    let kernel = gunzip(&cc37_dir().join("linux/Image.gz"));
    let rootfs = assemble_rootfs();
    let mut cpu = Cpu::boot_linux_disk(
        512 * 1024 * 1024,
        &kernel,
        rootfs,
        "console=ttyAMA0 root=/dev/vda rw init=/init",
    );
    let halt = cpu.run(40_000_000_000);
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!("---- guest console ----\n{console}\n---- end ----  (halt: {halt:?})");

    assert!(
        console.contains("CC37-DEVCONTAINER-UP"),
        "the arm64 devcontainer booted from its κ-disk virtio-blk rootfs"
    );
    // The stock linux-arm64 binary executed its own logic (a real computation).
    assert!(
        console.contains("CC37-COMPUTE:500500"),
        "the stock linux-arm64 binary ran its computation (sum 1..=1000 == 500500)"
    );
    // … and reports the guest architecture via the uname syscall.
    assert!(
        console.contains("CC37-ARCH:aarch64"),
        "the stock binary's uname syscall reports aarch64"
    );
    assert!(
        console.contains("Linux version 6.6.0"),
        "the stock binary read the real /proc/version over the mounted rootfs"
    );
    assert_eq!(
        halt,
        Halt::Exit(0),
        "the devcontainer powered off cleanly via PSCI (the init's reboot)"
    );
}
