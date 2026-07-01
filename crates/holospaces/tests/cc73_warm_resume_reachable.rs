//! `CC-73` — a warm-snapshotted **networked** server resumes and is **still reachable from the host**.
//!
//! The keystone for "heavy images run in practical time": a heavy image is booted ONCE, snapshotted while
//! serving, and every later run RESUMES the warm `.holo` in ~seconds instead of cold-booting for minutes.
//! CC-65 G4 already proved a warm resume keeps EXECUTING (an in-guest loopback server keeps serving), but
//! it had no external NIC — the κ-snapshot didn't serialize virtio-net device state (`Sys::snap` asserted
//! it was absent). This witnesses the missing half: with the virtio-net registers now serialized
//! (`VirtioNet::snap_regs`/`from_snapshot`) and re-attached on resume (`Cpu::reattach_net_forward`), a
//! server booted with a real NIC, snapshotted mid-listen, and resumed into a FRESH `Cpu` with a FRESH host
//! forward is reached by a real external `TcpStream` — the server's own bytes round-trip through the
//! re-attached κ-NAT. The authority is the response body a fresh NAT cannot fabricate.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// Spawn a host client that connects to `host_port`, sends an HTTP GET, and returns the response body via
/// `tx` if it contains BODY. Mirrors the CC-60 host-socket probe.
fn spawn_probe(host_port: u16, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        for _ in 0..80 {
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", host_port)) {
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = s.write_all(b"GET / HTTP/1.0\r\nHost: app\r\n\r\n");
                let mut resp = Vec::new();
                let mut chunk = [0u8; 512];
                loop {
                    match s.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => {
                            resp.extend_from_slice(&chunk[..n]);
                            if resp.windows(BODY.len()).any(|w| w == BODY.as_bytes()) {
                                break;
                            }
                        }
                        Err(_) => break,
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
#[ignore = "boots a NIC'd server, snapshots it, resumes into a fresh machine, reaches it from the host (heavy)"]
fn a_warm_snapshotted_server_resumes_and_is_reachable_from_the_host() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));

    // ── Phase 1: cold-boot the server with a NIC until it is listening, then warm-snapshot it. ──
    let mut ingress = StdIngress::new();
    let _host_a = ingress.forward(0, 8080).expect("bind forwarded host port (cold boot)");
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
    assert!(listening, "server image never brought eth0 up + listened before snapshot");
    // A few more chunks so the listen socket is fully settled before we freeze it.
    for _ in 0..10 {
        cpu.run(5_000_000);
    }
    let blob = cpu.snapshot_kappa_blob();
    eprintln!("CC-73: warm-snapshotted the listening server → {} MiB .holo", blob.len() / (1024 * 1024));
    drop(cpu); // the cold-boot machine (and its host forward) is gone — resume is fully independent.

    // ── Phase 2: resume into a FRESH machine, re-attach a FRESH host forward, reach it from the host. ──
    let t0 = Instant::now();
    let mut resumed = Cpu::new(0x1000);
    assert!(resumed.restore_kappa_blob(&blob), "restore the running-server .holo");
    let mut ingress2 = StdIngress::new();
    let host_b = ingress2.forward(0, 8080).expect("bind a fresh forwarded host port (resume)");
    assert!(
        resumed.reattach_net_forward(Box::new(NoEgress), Box::new(ingress2)),
        "re-attach net transports to the resumed device"
    );
    let resume_ready = t0.elapsed();

    let (tx, rx) = mpsc::channel::<String>();
    spawn_probe(host_b, tx);
    for _ in 0..600 {
        resumed.run(5_000_000);
        if let Ok(text) = rx.try_recv() {
            eprintln!(
                "CC-73: a WARM-RESUMED server image is REACHABLE from the host — got {BODY:?} over a real \
                 TcpStream through the RE-ATTACHED κ-NAT. resume-to-ready {resume_ready:?}, first byte at \
                 {:?} wall.",
                t0.elapsed()
            );
            assert!(text.contains(BODY));
            return;
        }
    }
    let con = String::from_utf8_lossy(resumed.console()).into_owned();
    let tail: Vec<_> = con.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
    panic!(
        "resumed server never became reachable from the host.\n  console tail:\n  {}",
        tail.join("\n  ")
    );
}
