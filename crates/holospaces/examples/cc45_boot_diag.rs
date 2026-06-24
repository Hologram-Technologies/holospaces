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
    // Cycles per slice (millions). Smaller = finer resolution through a stall.
    let per: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
        * 1_000_000;
    // A PID-unique log so concurrent/leftover runs can never corrupt each other's
    // output (a real hazard observed in this environment), plus a stable symlink
    // `boot-diag.log` to the most recent run for convenience.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let outp = dir.join(format!("boot-diag.{}.log", std::process::id()));
    let mut out = std::fs::File::create(&outp).unwrap();
    let link = dir.join("boot-diag.log");
    let _ = std::fs::remove_file(&link);
    let _ = std::os::unix::fs::symlink(outp.file_name().unwrap(), &link);
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
        // Run the slice in 64 sub-chunks, sampling RIP each, so a stuck slice
        // reveals its loop: collect the distinct top RIPs + the IF flag.
        let mut h = Halt::OutOfBudget;
        let mut samples: std::collections::BTreeMap<u64, u32> = std::collections::BTreeMap::new();
        let mut if_clear = 0u32;
        for _ in 0..64 {
            h = cpu.run(per / 64);
            *samples.entry(cpu.rip() & !0xfff).or_default() += 1;
            if cpu.rflags() & (1 << 9) == 0 {
                if_clear += 1;
            }
            if !matches!(h, Halt::OutOfBudget) {
                break;
            }
        }
        let mut top: Vec<_> = samples.into_iter().collect();
        top.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        let hot: Vec<String> = top
            .iter()
            .take(3)
            .map(|(p, c)| format!("{p:#x}:{c}"))
            .collect();
        let c = cpu.console();
        if c.len() > cur {
            out.write_all(&c[cur..]).unwrap();
            cur = c.len();
        }
        let st = cpu.mmu_stats();
        let u = cpu.uart_dbg();
        writeln!(
            out,
            "\n--- {}M cy {:.0}s prot=+{} nopage=+{} reval=+{} fills=+{} rip={:#x} \
             uart[thr={} ier={} iir={} lsr={} irq={}] ---",
            (s + 1) * per / 1_000_000,
            t0.elapsed().as_secs_f64(),
            st.protection_faults - prev.protection_faults,
            st.not_present_faults - prev.not_present_faults,
            st.tlb_revalidations - prev.tlb_revalidations,
            st.tlb_fills - prev.tlb_fills,
            cpu.rip(),
            u[0],
            u[1],
            u[2],
            u[3],
            u[4],
        )
        .unwrap();
        writeln!(out, "      hot_pages={hot:?} if_clear={if_clear}/64").unwrap();
        // Interrupt histogram (qemu -d int differential): every nonzero vector.
        let mut hist = String::from("      int[");
        for v in 0..=255u16 {
            let c = cpu.int_count(v as u8);
            if c > 0 {
                hist.push_str(&format!("{v:02x}={c} "));
            }
        }
        hist.push(']');
        writeln!(out, "{hist}").unwrap();
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
