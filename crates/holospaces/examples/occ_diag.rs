//! Boot the cc45 amd64 devcontainer via the occupancy-STREAMED κ-disk at a given
//! declared size (arg1 bytes), in cycle slices, printing console + rip progress —
//! to see whether a large disk boots (progresses) or hangs. Usage:
//! `cargo run --release --example occ_diag -- <disk_bytes> <slices>`
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
    let art = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc45");
    let disk: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8 << 30);
    let slices: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let layer = std::fs::read(art.join("rootfs/layer.tar.gz")).unwrap();
    let init = std::fs::read(art.join("init.sh")).unwrap();
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
        blob: &layer,
    }];

    let mut sparse: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut occ: Vec<u64> = Vec::new();
    let t0 = Instant::now();
    let geom = stream_ext4_image_bootable(&layers, &init, disk, |bi, b| {
        occ.push(bi);
        sparse.insert(bi, b.to_vec());
    })
    .unwrap();
    // The block device must span the IMAGE (image_len ≥ disk, by its metadata), not
    // the requested disk size — else the kernel rejects the root fs as oversized.
    let image_len = geom.image_len() as u64;
    let sector_count = image_len / 512;
    eprintln!(
        "assembled disk={} image_len={} occupied_blocks={} in {:.1}s",
        disk,
        image_len,
        occ.len(),
        t0.elapsed().as_secs_f64()
    );

    let read = |sector: u64, buf: &mut [u8]| {
        let bi = sector / 8;
        match sparse.get(&bi) {
            Some(b) => buf[..b.len()].copy_from_slice(b),
            None => buf.fill(0),
        }
    };
    let t1 = Instant::now();
    let mut cpu = Cpu::boot_linux_disk_occupancy_streamed(
        512 << 20,
        &gunzip(&art.join("linux/vmlinux.gz")),
        "console=ttyS0 root=/dev/vda rw init=/init virtio_mmio.device=0x200@0xd0000000:11 random.trust_cpu=on",
        Box::new(MemKappaStore::new()),
        sector_count,
        &occ,
        8,
        read,
    );
    eprintln!(
        "boot setup (from_occupancy_streamed) {:.1}s",
        t1.elapsed().as_secs_f64()
    );

    let per = 500_000_000u64;
    let mut cur = 0usize;
    for s in 0..slices {
        let h = cpu.run(per);
        let c = cpu.console();
        if c.len() > cur {
            print!("{}", String::from_utf8_lossy(&c[cur..]));
            cur = c.len();
        }
        eprintln!(
            "\n[slice {} {}M cy {:.0}s rip={:#x} halt={:?}]",
            s + 1,
            (s + 1) * per / 1_000_000,
            t1.elapsed().as_secs_f64(),
            cpu.rip(),
            h
        );
        if !matches!(h, Halt::OutOfBudget) {
            eprintln!("=== HALT {h:?} ===");
            break;
        }
    }
}
