//! Boot THIS repo's REAL devcontainer rootfs (exported from the actual built image,
//! Ubuntu 24.04 + the toolchain) on the x86-64 core and run the real gcc in-guest.
//! A feasibility probe (sliced console) before the formal witness. Usage:
//! `cargo run --release --example dogfood_probe -- <rootfs.tar> <init.sh> <slices>`
use hologram_store_mem::MemKappaStore;
use holospaces::assembly::{stream_ext4_image_bootable, Layer};
use holospaces::emulator::x64::{Cpu, Halt};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

fn gunzip(p: &Path) -> Vec<u8> {
    let mut o = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(p).unwrap()[..])
        .read_to_end(&mut o)
        .unwrap();
    o
}

fn main() {
    let rootfs_path = std::env::args().nth(1).expect("rootfs.tar path");
    let init_path = std::env::args().nth(2).expect("init.sh path");
    let slices: u64 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    let t0 = Instant::now();
    let rootfs = std::fs::read(&rootfs_path).expect("read rootfs.tar");
    let init = std::fs::read(&init_path).expect("read init.sh");
    // A static-busybox bootstrap layer overlaid on the real devcontainer rootfs (at
    // /usr/bin/busybox-static, the real dir — not /bin which Ubuntu symlinks). PID 1
    // is this static binary; the WORKLOAD is the dynamic toolchain.
    let bb = std::fs::read("/tmp/dogfood/busybox-layer.tar").expect("read busybox-layer.tar");
    eprintln!(
        "rootfs.tar {} MiB read in {:.1}s",
        rootfs.len() / 1024 / 1024,
        t0.elapsed().as_secs_f64()
    );
    // Uncompressed tar layers — the OCI `…tar` media type. Lower = the real rootfs,
    // upper = the busybox bootstrap.
    let layers = [
        Layer {
            media_type: "application/vnd.oci.image.layer.v1.tar",
            blob: &rootfs,
        },
        Layer {
            media_type: "application/vnd.oci.image.layer.v1.tar",
            blob: &bb,
        },
    ];

    const DISK: u64 = 8 * 1024 * 1024 * 1024;
    let mut sparse: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut occ: Vec<u64> = Vec::new();
    let ta = Instant::now();
    let geom = stream_ext4_image_bootable(&layers, &init, DISK, |bi, b| {
        occ.push(bi);
        sparse.insert(bi, b.to_vec());
    })
    .expect("assemble the real devcontainer rootfs into ext4");
    let image_len = geom.image_len();
    eprintln!(
        "assembled: image_len {} MiB, {} occupied blocks, in {:.1}s",
        image_len / 1024 / 1024,
        occ.len(),
        ta.elapsed().as_secs_f64()
    );

    let read = |sector: u64, buf: &mut [u8]| {
        let bi = sector / 8;
        match sparse.get(&bi) {
            Some(b) => buf[..b.len()].copy_from_slice(b),
            None => buf.fill(0),
        }
    };
    let kernel = gunzip(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../vv/artifacts/cc45/linux/vmlinux.gz")
            .as_path(),
    );
    let t1 = Instant::now();
    let mut cpu = Cpu::boot_linux_disk_occupancy_streamed(
        1024 * 1024 * 1024, // 1 GiB guest RAM (gcc needs headroom)
        &kernel,
        "console=ttyS0 root=/dev/vda rw init=/init virtio_mmio.device=0x200@0xd0000000:11 random.trust_cpu=on",
        Box::new(MemKappaStore::new()),
        image_len / 512,
        &occ,
        8,
        read,
    );
    eprintln!("boot setup {:.1}s", t1.elapsed().as_secs_f64());

    let per = 1_000_000_000u64;
    let mut cur = 0usize;
    for s in 0..slices {
        let h = cpu.run(per);
        let c = cpu.console();
        if c.len() > cur {
            print!("{}", String::from_utf8_lossy(&c[cur..]));
            cur = c.len();
        }
        eprintln!(
            "\n[slice {} {}B cy {:.0}s rip={:#x} halt={:?}]",
            s + 1,
            (s + 1),
            t1.elapsed().as_secs_f64(),
            cpu.rip(),
            h
        );
        if let Halt::Undefined(addr) = h {
            let bytes = cpu.peek(addr, 16);
            eprintln!("=== UNDEFINED at {addr:#x}: bytes = {bytes:02x?} ===");
            break;
        }
        if !matches!(h, Halt::OutOfBudget) {
            eprintln!("=== HALT {h:?} ===");
            break;
        }
    }
}
