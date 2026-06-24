//! Boot diagnostic for the CC-45 amd64 devcontainer on the x86-64 core.
//!
//! Runs the same boot the witness does, but in cycle slices, writing the guest
//! console plus [`MmuStats`](holospaces::emulator::x64::MmuStats) deltas per slice
//! to a persistent worktree file (`boot-diag.log`, never /tmp). This is the
//! standing tool for "the boot is slow / stuck" questions: a protection-fault or
//! demand-paging storm shows up immediately as one counter exploding while
//! retired instructions barely move. Usage: `cargo run --example cc45_boot_diag
//! -- [slice_count]`.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::x64::{Cpu, Halt};

fn art() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc45")
}
fn gunzip(p: &Path) -> Vec<u8> {
    let mut o = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(p).unwrap()[..])
        .read_to_end(&mut o)
        .unwrap();
    o
}

fn main() {
    let slices: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let outp = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../boot-diag.log");
    let mut out = std::fs::File::create(&outp).unwrap();
    let kernel = gunzip(&art().join("linux/vmlinux.gz"));
    let init = std::fs::read(art().join("init.sh")).unwrap();
    let layer = std::fs::read(art().join("rootfs/layer.tar.gz")).unwrap();
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
        blob: &layer,
    }];
    let rootfs = assemble_ext4_bootable(&layers, &init, 64 * 1024 * 1024).unwrap();
    let mut cpu = Cpu::boot_linux_disk(
        512 * 1024 * 1024,
        &kernel,
        rootfs,
        "earlyprintk=serial,ttyS0 console=ttyS0 root=/dev/vda rw init=/init \
         virtio_mmio.device=0x200@0xd0000000:11 random.trust_cpu=on",
    );
    let t0 = std::time::Instant::now();
    let mut cur = 0usize;
    let mut prev = cpu.mmu_stats();
    for s in 0..slices {
        let h = cpu.run(100_000_000);
        let c = cpu.console();
        if c.len() > cur {
            out.write_all(&c[cur..]).unwrap();
            cur = c.len();
        }
        let st = cpu.mmu_stats();
        writeln!(
            out,
            "\n--- {}M cy {:.0}s prot=+{} nopage=+{} revalidate=+{} ---",
            (s + 1) * 100,
            t0.elapsed().as_secs_f64(),
            st.protection_faults - prev.protection_faults,
            st.not_present_faults - prev.not_present_faults,
            st.tlb_revalidations - prev.tlb_revalidations,
        )
        .unwrap();
        prev = st;
        out.flush().unwrap();
        if !matches!(h, Halt::OutOfBudget) {
            writeln!(out, "=== HALT {h:?} stats={st:?} ===").unwrap();
            out.flush().unwrap();
            return;
        }
    }
    writeln!(out, "=== budget exhausted, stats={:?} ===", cpu.mmu_stats()).unwrap();
}
