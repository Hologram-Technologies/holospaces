//! `CC-75` — share a running server BY κ: a NIC'd server is sealed into a content-addressed κ-snapshot,
//! and a peer holding only that **one κ** (plus the store) resumes it and the server is REACHABLE.
//!
//! This is the bridge from `holo run` (a local warm `.holo`) to the north star — "open a κ-link, a live
//! reachable app appears" — via the κ-manifest path rather than a self-contained blob. `Cpu::seal_kappa`
//! content-addresses the machine (CPU + device state, incl. the virtio-net registers from CC-73, + the
//! per-page BLAKE3 κ list) into the store and returns the snapshot κ; `Cpu::resume_kappa` fetches that
//! manifest by κ, VERIFIES it (L5), and fetches + verifies each RAM page — the exact entry point a browser
//! peer uses to `open(κ)`. Here we prove the resumed machine, with a re-attached forward, serves a real
//! external client — the network device survives the κ round-trip, not just the flat blob (CC-73).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hologram_store_mem::MemKappaStore;
use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::net::{NoEgress, StdIngress};
use holospaces::emulator::x64::Cpu;
use holospaces::image_init::{image_init, RunConfig};

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
const BODY: &str = "HELLO-FROM-HOLO-REACHABLE";

fn server_rootfs() -> Vec<u8> {
    let layer = std::fs::read(art("vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz")).unwrap();
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).expect("compile image-init.c first");
    let script = format!(
        "/bin/busybox ip link set eth0 up\n\
         /bin/busybox ip addr add 10.0.2.15/24 dev eth0\n\
         /bin/busybox ip route add default via 10.0.2.2 2>/dev/null\n\
         printf 'HTTP/1.0 200 OK\\r\\nContent-Length: 25\\r\\n\\r\\n{BODY}' > /resp\n\
         ( while true; do /bin/busybox nc -l -p 8080 < /resp >/dev/null 2>&1; done ) &\n\
         /bin/busybox sleep 1\n\
         echo HOLO-LISTENING\n\
         /bin/busybox sleep 1000000\n"
    );
    let cfg = RunConfig {
        argv: vec!["/bin/sh".into(), "-c".into(), script],
        env: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()],
        workdir: "/".into(),
        uid: 0,
        gid: 0,
        net_up: false,
    };
    let init = image_init(&template, &cfg).expect("patch image-init (reachable server)");
    assemble_ext4_bootable(
        &[Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer }],
        &init,
        256 * 1024 * 1024,
    )
    .expect("assemble reachable-server rootfs")
}

fn spawn_probe(host_port: u16, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        for _ in 0..80 {
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
                    if resp.windows(BODY.len()).any(|w| w == BODY.as_bytes()) {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&resp).into_owned();
                if text.contains(BODY) {
                    let _ = tx.send(text);
                    return;
                }
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
}

#[test]
#[ignore = "boots a NIC'd server, seals it by κ, resumes it from that κ, reaches it from the host (heavy)"]
fn a_server_sealed_by_kappa_resumes_reachable() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let store = MemKappaStore::new();

    // ── boot the server with a NIC until it is listening, then SEAL it by κ into the store. ──
    let mut ingress = StdIngress::new();
    let _host_a = ingress.forward(0, 8080).expect("bind forwarded host port (seal)");
    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, server_rootfs(), NIC_CMDLINE);
    cpu.attach_net_forward(Box::new(NoEgress), Box::new(ingress));
    let mut listening = false;
    for _ in 0..600 {
        cpu.run(5_000_000);
        if String::from_utf8_lossy(cpu.console()).contains("HOLO-LISTENING") {
            listening = true;
            break;
        }
    }
    assert!(listening, "server never listened before seal");
    for _ in 0..30 {
        cpu.run(5_000_000); // drain so the seal captures an idle-listening server
    }
    let kappa = cpu.seal_kappa(&store).expect("seal the running server into a κ-snapshot");
    eprintln!("CC-75: sealed the running server → snapshot κ (share this one label).");
    drop(cpu); // the sealing machine is gone — the peer has ONLY the κ + the store.

    // ── a peer with ONLY the κ (+ the store) resumes it, re-attaches a forward, and is reachable. ──
    let t0 = Instant::now();
    let mut peer = Cpu::new(0x1000);
    assert!(peer.resume_kappa(&kappa, &store), "resume the server from its snapshot κ (L5-verified)");
    let mut ingress2 = StdIngress::new();
    let host_b = ingress2.forward(0, 8080).expect("bind a fresh forwarded host port (resume)");
    assert!(
        peer.reattach_net_forward(Box::new(NoEgress), Box::new(ingress2)),
        "re-attach net transports to the κ-resumed device"
    );
    let resume_ready = t0.elapsed();

    let (tx, rx) = mpsc::channel::<String>();
    spawn_probe(host_b, tx);
    for _ in 0..600 {
        peer.run(5_000_000);
        if let Ok(text) = rx.try_recv() {
            eprintln!(
                "CC-75: a κ-RESUMED server is REACHABLE from the host — got {BODY:?} through the \
                 re-attached κ-NAT. resume-to-ready {resume_ready:?}, first byte {:?}.",
                t0.elapsed()
            );
            assert!(text.contains(BODY));
            return;
        }
    }
    let con = String::from_utf8_lossy(peer.console()).into_owned();
    let tail: Vec<_> = con.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
    panic!("κ-resumed server never became reachable.\n  console tail:\n  {}", tail.join("\n  "));
}
