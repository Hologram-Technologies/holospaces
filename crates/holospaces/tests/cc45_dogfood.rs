//! CC-45 dogfood — **holospaces builds in its OWN, unmodified devcontainer.**
//!
//! The decisive #13 reference: not a fixture, not TinyCC — THIS repo's real
//! `.devcontainer` (Ubuntu 24.04 + the unmodified toolchain), built by the Dev
//! Container CLI exactly as Codespaces/Gitpod would, boots on the holospaces x86-64
//! core, and the real **gcc 13.3** (cc1 → as → ld, over glibc 2.39 + ld.so) compiles
//! a C program in-guest — and the guest runs the binary it built.
//!
//! The rootfs is multi-GiB, so it is NOT a committed fixture: the `cc45-dogfood`
//! vv suite builds this repo's devcontainer image, exports its rootfs, and points
//! this `#[ignore]`d witness at it via `CC45_DOGFOOD_ROOTFS`. Run it through the
//! suite, never bare. The static busybox (the CC-45 fixture) is overlaid only as the
//! PID-1 bootstrap (it has no libc dependency); the WORKLOAD is the dynamic toolchain.

use hologram_store_mem::MemKappaStore;
use holospaces::assembly::{stream_ext4_image_bootable, Layer};
use holospaces::emulator::x64::{Cpu, Halt};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

fn cc45_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc45")
}

fn gunzip(p: &Path) -> Vec<u8> {
    let mut o = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(p).unwrap()[..])
        .read_to_end(&mut o)
        .unwrap();
    o
}

/// A minimal uncompressed USTAR archive (the OCI `…tar` layer media type) of
/// `(path, bytes, mode)` entries.
fn ustar(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let oct = |f: &mut [u8], v: u64| {
        let s = format!("{:0w$o}", v, w = f.len() - 1);
        f[..s.len()].copy_from_slice(s.as_bytes());
    };
    let mut tar = Vec::new();
    for (path, data, mode) in entries {
        let mut h = [0u8; 512];
        h[..path.len()].copy_from_slice(path.as_bytes());
        oct(&mut h[100..108], u64::from(*mode));
        oct(&mut h[124..136], data.len() as u64);
        h[156] = b'0';
        h[257..263].copy_from_slice(b"ustar\0");
        h[263] = b'0';
        h[264] = b'0';
        h[148..156].fill(b' ');
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        oct(&mut h[148..155], u64::from(sum));
        h[155] = b' ';
        tar.extend_from_slice(&h);
        tar.extend_from_slice(data);
        tar.extend(std::iter::repeat_n(
            0u8,
            data.len().div_ceil(512) * 512 - data.len(),
        ));
    }
    tar.extend([0u8; 1024]);
    tar
}

/// PID 1: the static busybox bootstraps the pseudo-filesystems, then the REAL gcc
/// the devcontainer ships compiles a program and the guest runs the result.
const DOGFOOD_INIT: &[u8] = b"#!/usr/bin/busybox-static sh\n\
    BB=/usr/bin/busybox-static\n\
    $BB mount -t proc proc /proc 2>/dev/null\n\
    $BB mount -t sysfs sys /sys 2>/dev/null\n\
    $BB mount -t tmpfs tmp /tmp 2>/dev/null\n\
    export PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n\
    export HOME=/root\n\
    echo DOGFOOD-PID1-UP\n\
    echo \"uname=$(/usr/bin/uname -m)\"\n\
    /usr/bin/gcc --version 2>&1 | $BB head -1\n\
    $BB cat > /tmp/h.c <<'CEOF'\n\
#include <stdio.h>\n\
int main(void){ printf(\"DOGFOOD-GCC-BUILT:%d\\n\", 6*7); return 0; }\n\
CEOF\n\
    /usr/bin/gcc /tmp/h.c -o /tmp/h\n\
    echo \"gcc-compile-rc:$?\"\n\
    /tmp/h\n\
    echo \"gcc-ran-rc:$?\"\n\
    echo DOGFOOD-DONE\n\
    $BB poweroff -f\n";

#[test]
#[ignore = "builds + boots THIS repo's real devcontainer; needs docker — run by the cc45-dogfood suite"]
fn holospaces_builds_in_its_own_real_devcontainer() {
    // The suite builds the devcontainer image, exports its rootfs (uncompressed tar),
    // and hands us the path. Bare `cargo test` does not set this — run via the suite.
    let rootfs_path = std::env::var("CC45_DOGFOOD_ROOTFS").expect(
        "CC45_DOGFOOD_ROOTFS — the exported real-devcontainer rootfs; run via the cc45-dogfood suite",
    );
    let rootfs = std::fs::read(&rootfs_path).expect("read the exported devcontainer rootfs");

    // The static-busybox PID-1 bootstrap, overlaid at /usr/bin/busybox-static (the
    // real dir — not /bin, which Ubuntu symlinks). It has no libc dependency, so it
    // boots regardless; the dynamic toolchain is the real workload.
    let busybox = std::fs::read(cc45_dir().join("rootfs/busybox")).expect("cc45 static busybox");
    let bb_layer = ustar(&[("usr/bin/busybox-static", &busybox, 0o755)]);

    let layers = [
        Layer {
            media_type: "application/vnd.oci.image.layer.v1.tar",
            blob: &rootfs,
        },
        Layer {
            media_type: "application/vnd.oci.image.layer.v1.tar",
            blob: &bb_layer,
        },
    ];

    // Assemble the (multi-GiB) rootfs onto a build-capable disk, sparse, recording
    // occupancy; boot it O(content) — the same κ-disk path the deployed peer uses.
    const DISK: u64 = 16 * 1024 * 1024 * 1024;
    let mut sparse: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut occ: Vec<u64> = Vec::new();
    let geom = stream_ext4_image_bootable(&layers, DOGFOOD_INIT, DISK, |bi, b| {
        occ.push(bi);
        sparse.insert(bi, b.to_vec());
    })
    .expect("assemble the real devcontainer rootfs into a bootable ext4");
    let image_len = geom.image_len();

    let read = |sector: u64, buf: &mut [u8]| {
        let bi = sector / 8;
        match sparse.get(&bi) {
            Some(b) => buf[..b.len()].copy_from_slice(b),
            None => buf.fill(0),
        }
    };
    let kernel = gunzip(&cc45_dir().join("linux/vmlinux.gz"));
    let mut cpu = Cpu::boot_linux_disk_occupancy_streamed(
        1024 * 1024 * 1024, // 1 GiB guest RAM — gcc needs headroom
        &kernel,
        "console=ttyS0 root=/dev/vda rw init=/init \
         virtio_mmio.device=0x200@0xd0000000:11 random.trust_cpu=on",
        Box::new(MemKappaStore::new()),
        image_len / 512,
        &occ,
        8,
        read,
    );

    // Run in slices; stop as soon as the guest finishes (or halts).
    let mut halted = false;
    for _ in 0..120 {
        if !matches!(cpu.run(1_000_000_000), Halt::OutOfBudget) {
            halted = true;
            break;
        }
        if cpu.console().windows(13).any(|w| w == b"DOGFOOD-DONE\n") {
            break;
        }
    }
    let console = String::from_utf8_lossy(cpu.console());
    eprintln!("---- real devcontainer dogfood ----\n{console}\n---- end ----  halted={halted}");

    assert!(
        console.contains("uname=x86_64"),
        "the real devcontainer's dynamic glibc binaries run on the x86-64 core"
    );
    assert!(
        console.contains("gcc-compile-rc:0"),
        "the real gcc (cc1 → as → ld) compiled the program in-guest"
    );
    assert!(
        console.contains("DOGFOOD-GCC-BUILT:42"),
        "the guest RAN the binary the real toolchain just built — build-capable for real"
    );
}
