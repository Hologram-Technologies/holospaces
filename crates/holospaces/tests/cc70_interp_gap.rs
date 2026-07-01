//! `CC-70` Step 0 — pin the interpreter (`python`) core gap: boot python to the wild-pointer fault and
//! capture the EXACT faulting rip + the instruction BYTES there (via `peek_code`, no core edit) + the fault
//! ring. The `cr2=0x8080808080808080` is musl memchr/strlen's SWAR "high-bit" mask being dereferenced as a
//! pointer — this dumps the deref instruction so we can trace which op put the mask into an address register.

use std::io::Read;
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::net::{NoEgress, StdIngress};
use holospaces::emulator::x64::{drain_exc_trace, Cpu};
use holospaces::image_init::{image_init, run_config_from_oci, RunConfig};
use holospaces::import::{parse_image_ref, pull_image};

fn art(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}
fn gunzip(p: PathBuf) -> Vec<u8> {
    let raw = std::fs::read(&p).unwrap();
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out).unwrap();
    out
}
const NIC_CMDLINE: &str = "virtio_mmio.device=0x200@0xd0000400:12 virtio_mmio.device=0x200@0xd0000000:11 \
                           console=ttyS0 root=/dev/vda rw init=/init random.trust_cpu=on norandmaps \
                           nmi_watchdog=0 nowatchdog tsc=reliable";

#[test]
#[ignore = "LIVE-pulls python:3-alpine and captures the fault locus (network + heavy)"]
fn pin_python_fault_locus() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).unwrap();
    let store = MemKappaStore::new();
    let img = pull_image(&store, &parse_image_ref("python:3-alpine").unwrap(), holospaces::Arch::X64)
        .expect("pull python:3-alpine");
    let rc = run_config_from_oci(&store.get(img.config()).unwrap().unwrap().as_ref().to_vec()).unwrap_or_default();
    // Run a trivial python one-liner that exercises string handling then serves — but the fault happens in
    // interpreter startup, so `python3 -c` is enough to trip it.
    let script = "ip addr add 10.0.2.15/24 dev eth0 2>/dev/null; ip link set eth0 up 2>/dev/null; \
                  echo HOLO-NET-UP; exec python3 -m http.server 8080";
    let cfg = RunConfig {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: rc.env.clone(),
        workdir: "/".into(),
        uid: 0,
        gid: 0,
        net_up: false,
    };
    let init = image_init(&template, &cfg).unwrap();
    let layers: Vec<Vec<u8>> = img.layers().iter().map(|k| store.get(k).unwrap().unwrap().as_ref().to_vec()).collect();
    let media = img.layer_media_types();
    let rootfs = assemble_ext4_bootable(
        &layers.iter().zip(media.iter()).map(|(b, mt)| Layer { media_type: mt, blob: b }).collect::<Vec<_>>(),
        &init,
        768 * 1024 * 1024,
    )
    .unwrap();

    let mut ingress = StdIngress::new();
    let _ = ingress.forward(0, 8080);
    let mut cpu = Cpu::boot_linux_disk(1024 * 1024 * 1024, &kernel, rootfs, NIC_CMDLINE);
    cpu.attach_net_forward(Box::new(NoEgress), Box::new(ingress));
    let _ = drain_exc_trace();

    let mut wild_rips: Vec<u64> = Vec::new();
    let ran_ok = false;
    for _ in 0..1600 {
        cpu.run(5_000_000);
        let _con = String::from_utf8_lossy(cpu.console());
        // Scan the fault ring for the wild-pointer signature.
        for line in drain_exc_trace() {
            if line.contains("cr2=0x8080808080808080") || line.contains("cr2=0x8080808080808080") {
                if let Some(idx) = line.find("rip=0x") {
                    if let Ok(rip) = u64::from_str_radix(line[idx + 6..].split(' ').next().unwrap_or(""), 16) {
                        if !wild_rips.contains(&rip) {
                            wild_rips.push(rip);
                        }
                    }
                }
            }
        }
        if wild_rips.len() >= 3 {
            break;
        }
    }

    eprintln!("CC-70: python ran_ok={ran_ok}  wild-ptr faulting rips={:#x?}", wild_rips);
    for rip in &wild_rips {
        let bytes = cpu.peek_code(*rip, 16);
        eprintln!("  rip={rip:#x}  insn bytes = {:02x?}", bytes);
    }
    // Final fault tail for context.
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let tail: Vec<_> = con.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect();
    eprintln!("  console tail:\n  {}", tail.join("\n  "));
    assert!(ran_ok || !wild_rips.is_empty(), "neither ran nor captured a wild-ptr fault");
}
