//! `CC-74` — a REAL registry image, pulled live, serves over HTTP; snapshotted while serving and resumed
//! into a fresh machine, it serves AGAIN — the full "boot once, resume forever, reachable" chain that
//! makes heavy images practical (`holo run`).
//!
//! This is the standing witness behind the `holo run` warm-snapshot cache. Unlike CC-73 (a local
//! alpine+nc fixture), it drives the whole product path on a LIVE image: pull → κ-ingest (L5) →
//! `run_config_from_oci` (the image's own entrypoint) → net-up-in-init boot with a NIC → an external host
//! `TcpStream` gets the image's REAL HTTP response (COLD) → `snapshot_kappa_blob` → restore into a fresh
//! `Cpu` + `reattach_net_forward` → the external client gets the REAL response AGAIN (WARM), with warm
//! resume-to-first-byte far below the cold boot. nginx:alpine (musl) keeps the cold boot short.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::net::{NoEgress, StdIngress};
use holospaces::emulator::x64::Cpu;
use holospaces::image_init::{image_init, run_config_from_oci, RunConfig};
use holospaces::import::{parse_image_ref, pull_image};

fn art(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}
fn gunzip(p: PathBuf) -> Vec<u8> {
    let raw = std::fs::read(&p).unwrap_or_else(|_| panic!("read {}", p.display()));
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out).unwrap();
    out
}

const NIC_CMDLINE: &str = "virtio_mmio.device=0x200@0xd0000400:12 virtio_mmio.device=0x200@0xd0000000:11 \
                           console=ttyS0 root=/dev/vda rw init=/init random.trust_cpu=on norandmaps \
                           nmi_watchdog=0 nowatchdog tsc=reliable";

/// A host client that GETs `/` through the forward once `armed`, sending the reply's first line back.
fn spawn_http_probe(host_port: u16, armed: std::sync::Arc<std::sync::atomic::AtomicBool>, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        while !armed.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(100));
        }
        for _ in 0..120 {
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", host_port)) {
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = s.write_all(b"GET / HTTP/1.0\r\nHost: app\r\n\r\n");
                let mut resp = Vec::new();
                let mut chunk = [0u8; 512];
                while let Ok(n) = s.read(&mut chunk) {
                    if n == 0 || resp.len() > 4096 {
                        break;
                    }
                    resp.extend_from_slice(&chunk[..n]);
                }
                let text = String::from_utf8_lossy(&resp).into_owned();
                if text.contains("200") || text.to_ascii_lowercase().contains("html") {
                    let _ = tx.send(text.lines().next().unwrap_or("").to_string());
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
}

/// Run `cpu` until the host probe (armed on `HOLO-NET-UP`) gets a real HTTP response, or budget expires.
/// Returns `Some((first_line, elapsed))`.
fn serve_and_probe(cpu: &mut Cpu, host_port: u16, chunks: u32) -> Option<(String, Duration)> {
    let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<String>();
    spawn_http_probe(host_port, armed.clone(), tx);
    let t0 = Instant::now();
    let mut net_up = false;
    for _ in 0..chunks {
        cpu.run(5_000_000);
        if !net_up && String::from_utf8_lossy(cpu.console()).contains("HOLO-NET-UP") {
            net_up = true;
            armed.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Ok(line) = rx.try_recv() {
            return Some((line, t0.elapsed()));
        }
    }
    None
}

#[test]
#[ignore = "LIVE-pulls nginx:alpine, boots it, serves, snapshots, resumes, serves again (network + heavy)"]
fn a_live_image_serves_then_warm_resumes_and_serves_again() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).expect("compile image-init.c first");
    let store = MemKappaStore::new();
    let iref = parse_image_ref("nginx:alpine").expect("parse nginx:alpine");
    let img = pull_image(&store, &iref, holospaces::Arch::X64).expect("live-pull nginx:alpine (amd64)");
    let cfg_bytes = store.get(img.config()).unwrap().unwrap().as_ref().to_vec();
    let rc = run_config_from_oci(&cfg_bytes).expect("httpd image declares an Entrypoint/Cmd");

    // net-up-in-init: the freestanding init brings eth0 up, then DIRECT-execs the image's entrypoint.
    let cfg = RunConfig {
        argv: rc.argv.clone(),
        env: rc.env.clone(),
        workdir: if rc.workdir.is_empty() { "/".into() } else { rc.workdir.clone() },
        uid: 0,
        gid: 0,
        net_up: true,
    };
    let init = image_init(&template, &cfg).expect("patch image-init with httpd's entrypoint");
    let layer_bytes: Vec<Vec<u8>> =
        img.layers().iter().map(|k| store.get(k).unwrap().unwrap().as_ref().to_vec()).collect();
    let media = img.layer_media_types();
    let layers: Vec<Layer> =
        layer_bytes.iter().zip(media.iter()).map(|(b, mt)| Layer { media_type: mt, blob: b }).collect();
    let rootfs = assemble_ext4_bootable(&layers, &init, 768 * 1024 * 1024).expect("assemble httpd rootfs");

    // ── COLD: boot with a NIC + forward; an external host client gets the REAL response. ──
    let mut ingress = StdIngress::new();
    let host_a = ingress.forward(0, 80).expect("forward host → guest :80 (cold)");
    let mut cpu = Cpu::boot_linux_disk(1024 * 1024 * 1024, &kernel, rootfs, NIC_CMDLINE);
    cpu.attach_net_forward(Box::new(NoEgress), Box::new(ingress));
    let (cold_line, cold_dt) =
        serve_and_probe(&mut cpu, host_a, 2000).expect("httpd never served over HTTP (cold)");
    eprintln!("CC-74 COLD: nginx:alpine served {cold_line:?} in {cold_dt:?}");

    // Drain: let the probe's connection fully close in the guest so the snapshot captures an idle-
    // listening server, not one mid-teardown (a half-open connection resumes badly).
    for _ in 0..30 {
        cpu.run(5_000_000);
    }

    // ── snapshot the serving machine, then resume into a FRESH one with a FRESH forward. ──
    let blob = cpu.snapshot_kappa_blob();
    drop(cpu);
    let t_resume = Instant::now();
    let mut resumed = Cpu::new(0x1000);
    assert!(resumed.restore_kappa_blob(&blob), "restore the serving-httpd .holo");
    let mut ingress2 = StdIngress::new();
    let host_b = ingress2.forward(0, 80).expect("forward host → guest :80 (resume)");
    assert!(
        resumed.reattach_net_forward(Box::new(NoEgress), Box::new(ingress2)),
        "re-attach net transports to the resumed device"
    );
    let resume_ready = t_resume.elapsed();

    // ── WARM: the resumed machine serves the REAL response AGAIN. ──
    let (warm_line, warm_dt) =
        serve_and_probe(&mut resumed, host_b, 600).expect("resumed nginx never served over HTTP (warm)");
    eprintln!(
        "CC-74 WARM: resume-to-ready {resume_ready:?}, served {warm_line:?} in {warm_dt:?} \
         (blob {} MiB). vs COLD {cold_dt:?} — warm resume is the practical-time path.",
        blob.len() / (1024 * 1024)
    );
    assert!(warm_line.contains("200") || warm_line.to_ascii_lowercase().contains("http"));
    assert!(
        warm_dt < cold_dt,
        "warm resume ({warm_dt:?}) should reach first byte faster than the cold boot ({cold_dt:?})"
    );
}
