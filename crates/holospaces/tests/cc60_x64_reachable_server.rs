//! `CC-60` (behavioral) — a real server image is REACHABLE from the HOST on x86-64.
//!
//! The behavioral completion of CC-60's control-plane parity: the CC-65 pipeline boots a server image
//! with a virtio-net NIC, the guest's real app binds `0.0.0.0:8080`, and a real host `TcpStream` reaches
//! it through the κ-native NAT ingress (`StdIngress::forward` → `poll_ingress` → the guest's virtio-net
//! RX). The authority is the server's own response bytes round-tripped through a real external client —
//! something the NAT cannot fake. Mirrors the riscv `cc21_port_forward` shape, for x64.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::net::{NoEgress, StdIngress};
use holospaces::emulator::x64::Cpu;
use holospaces::image_init::{image_init, run_config_from_oci, RunConfig};

fn art(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}
fn gunzip(p: PathBuf) -> Vec<u8> {
    use std::io::Read as _;
    let raw = std::fs::read(&p).unwrap_or_else(|_| panic!("read {}", p.display()));
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out).unwrap();
    out
}

// blk @0xd0000000:11 AND net @0xd0000400:12 — the guest probes both virtio-mmio devices.
const NIC_CMDLINE: &str = "virtio_mmio.device=0x200@0xd0000400:12 virtio_mmio.device=0x200@0xd0000000:11 \
                           console=ttyS0 root=/dev/vda rw init=/init random.trust_cpu=on norandmaps \
                           nmi_watchdog=0 nowatchdog tsc=reliable";

const BODY: &str = "HELLO-FROM-HOLO-REACHABLE";

fn server_rootfs() -> Vec<u8> {
    let layer = std::fs::read(art("vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz")).unwrap();
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).expect("compile image-init.c first");
    // The image's entrypoint: bring eth0 up (static 10.0.2.15), then serve BODY on 0.0.0.0:8080 forever.
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

/// A host client reaches the in-guest server through a forwarded port — over a REAL host socket.
#[test]
#[ignore = "boots a server image with a NIC and reaches it from the host (slow, heavy)"]
fn a_real_amd64_server_image_is_reachable_from_the_host() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));

    let mut ingress = StdIngress::new();
    let host_port = ingress.forward(0, 8080).expect("bind a forwarded host port");

    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, server_rootfs(), NIC_CMDLINE);
    cpu.attach_net_forward(Box::new(NoEgress), Box::new(ingress));

    // Run in chunks; when the guest server is listening, a host client connects through the forward on a
    // separate thread (it reaches the host listener while the emulator keeps servicing the NAT/ingress).
    let (tx, rx) = mpsc::channel::<String>();
    let mut client_started = false;
    let t0 = std::time::Instant::now();
    let i0 = cpu.insns();
    for _ in 0..600 {
        cpu.run(5_000_000);
        let console = String::from_utf8_lossy(cpu.console());
        if !client_started && console.contains("HOLO-LISTENING") {
            client_started = true;
            let tx = tx.clone();
            std::thread::spawn(move || {
                for _ in 0..60 {
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
        if let Ok(text) = rx.try_recv() {
            let insns = cpu.insns() - i0;
            eprintln!(
                "CC-60: a real amd64 server image is REACHABLE from the host — got {BODY:?} over a real \
                 TcpStream through the κ-NAT forward. cost: {insns} guest insns, {:?} wall.",
                t0.elapsed()
            );
            assert!(text.contains(BODY));
            return;
        }
    }
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let tail: Vec<_> = con.lines().rev().take(12).collect::<Vec<_>>().into_iter().rev().collect();
    panic!(
        "host client never reached the in-guest server (listening={client_started}).\n  tail:\n  {}",
        tail.join("\n  ")
    );
}

/// The hermetic dual of the above: the same in-guest server, reached over the IN-PROCESS loopback bridge
/// (CC-33 — no host socket). Proves the guest's virtio-net RX + the NAT ingress deliver a real request to
/// the app and return its bytes, with no external port. (The named witness the CC-60 target promotes on.)
#[test]
#[ignore = "boots a server image with a NIC and reaches it over the loopback bridge (slow, heavy)"]
fn a_real_amd64_server_image_is_reachable_over_the_loopback_bridge() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, server_rootfs(), NIC_CMDLINE);
    cpu.attach_net(Box::new(NoEgress));
    assert!(cpu.enable_loopback(), "enable the in-process loopback bridge");

    let mut listening = false;
    for _ in 0..600 {
        cpu.run(5_000_000);
        if String::from_utf8_lossy(cpu.console()).contains("HOLO-LISTENING") {
            listening = true;
            break;
        }
    }
    assert!(listening, "the server image never brought eth0 up + listened");

    let id = cpu.dial_guest(8080).expect("dial the guest server on 8080 over the loopback bridge");
    cpu.guest_send(id, b"GET / HTTP/1.0\r\nHost: app\r\n\r\n");
    let mut resp = Vec::new();
    for _ in 0..300 {
        cpu.run(2_000_000);
        resp.extend_from_slice(&cpu.guest_recv(id));
        if resp.windows(BODY.len()).any(|w| w == BODY.as_bytes()) {
            break;
        }
    }
    let text = String::from_utf8_lossy(&resp).into_owned();
    assert!(text.contains(BODY), "loopback client never got the server body. got: {text:?}");
    eprintln!("CC-60: the in-guest server is reachable over the in-process loopback bridge — got {BODY:?}.");
}

/// G5 — a REAL image pulled LIVE from docker.io (nginx:alpine, amd64) boots its OWN entrypoint and is
/// reachable from the host. Pull → κ-ingest (L5 verify) → run_config_from_oci (the image's real
/// Entrypoint/Cmd) → a thin network-prelude wrapper (a container runtime brings the NIC up, then execs the
/// image's entrypoint) → assemble → boot with a NIC → a host client fetches nginx's real response. This is
/// "docker run nginx", opened as a reachable endpoint on the κ substrate.
#[test]
#[ignore = "LIVE-pulls nginx:alpine from docker.io and boots it (network + very heavy)"]
fn real_nginx_image_boots_and_is_reachable_from_the_host() {
    use hologram_store_mem::MemKappaStore;
    use hologram_substrate_core::KappaStore;
    use holospaces::import::{parse_image_ref, pull_image};

    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let store = MemKappaStore::new();
    let image = parse_image_ref("nginx:alpine").expect("parse nginx:alpine");
    let img = pull_image(&store, &image, holospaces::Arch::X64).expect("live-pull nginx:alpine (amd64)");

    let cfg_bytes = store.get(img.config()).unwrap().unwrap().as_ref().to_vec();
    let run = run_config_from_oci(&cfg_bytes).expect("nginx image declares an Entrypoint/Cmd");
    eprintln!("CC-60 G5: nginx image's real argv = {:?}", run.argv);

    // Layers, in order, straight from the κ-store (each L5-verified at ingest).
    let layer_bytes: Vec<Vec<u8>> =
        img.layers().iter().map(|k| store.get(k).unwrap().unwrap().as_ref().to_vec()).collect();
    let media = img.layer_media_types();
    let layers: Vec<Layer> =
        layer_bytes.iter().zip(media.iter()).map(|(b, mt)| Layer { media_type: mt, blob: b }).collect();

    // Network prelude (the container-runtime's job): bring eth0 up, then EXEC the image's real entrypoint.
    let shq = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let argv_str = run.argv.iter().map(|a| shq(a)).collect::<Vec<_>>().join(" ");
    let script = format!(
        "ip addr add 10.0.2.15/24 dev eth0 2>/dev/null; ip link set eth0 up 2>/dev/null; \
         ip route add default via 10.0.2.2 2>/dev/null; echo HOLO-NET-UP; exec {argv_str}"
    );
    let cfg = RunConfig {
        argv: vec!["/bin/sh".into(), "-c".into(), script],
        env: run.env.clone(),
        workdir: if run.workdir.is_empty() { "/".into() } else { run.workdir.clone() },
        uid: 0,
        gid: 0,
        net_up: false,
    };
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).expect("compile image-init.c first");
    let init = image_init(&template, &cfg).expect("patch image-init with nginx's entrypoint");
    let rootfs = assemble_ext4_bootable(&layers, &init, 768 * 1024 * 1024).expect("assemble nginx rootfs");

    let mut ingress = StdIngress::new();
    let host_port = ingress.forward(0, 80).expect("forward host → guest :80");
    let mut cpu = Cpu::boot_linux_disk(1024 * 1024 * 1024, &kernel, rootfs, NIC_CMDLINE);
    cpu.attach_net_forward(Box::new(NoEgress), Box::new(ingress));

    let (tx, rx) = mpsc::channel::<String>();
    let mut client_started = false;
    for _ in 0..1200 {
        cpu.run(5_000_000);
        let console = String::from_utf8_lossy(cpu.console());
        if !client_started && console.contains("HOLO-NET-UP") {
            client_started = true;
            let tx = tx.clone();
            std::thread::spawn(move || {
                for _ in 0..120 {
                    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", host_port)) {
                        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n");
                        let mut resp = Vec::new();
                        let mut chunk = [0u8; 1024];
                        loop {
                            match s.read(&mut chunk) {
                                Ok(0) => break,
                                Ok(n) => {
                                    resp.extend_from_slice(&chunk[..n]);
                                    if resp.len() > 32 && (resp.windows(6).any(|w| w == b"nginx/") || resp.windows(13).any(|w| w == b"Welcome to ng")) {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let text = String::from_utf8_lossy(&resp).into_owned();
                        if text.contains("nginx") || text.contains("200 OK") {
                            let _ = tx.send(text);
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(400));
                }
            });
        }
        if let Ok(text) = rx.try_recv() {
            let first_line = text.lines().next().unwrap_or("");
            eprintln!("CC-60 G5: LIVE nginx:alpine is REACHABLE from the host — response: {first_line:?} (server header contains nginx).");
            assert!(text.contains("nginx") || text.contains("200 OK"));
            return;
        }
    }
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let tail: Vec<_> = con.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
    panic!(
        "nginx never became reachable (net_up={client_started}). boot tail (the gap map):\n  {}",
        tail.join("\n  ")
    );
}

/// G4 breadth — a SECOND, protocol-different real image: redis:alpine (RESP, not HTTP). Pulled live,
/// booted with its OWN entrypoint (`docker-entrypoint.sh redis-server`, + `--protected-mode no` so the NAT
/// client — which appears from the gateway, non-loopback — isn't refused), reached from the host with a
/// real `PING` → `+PONG`. Proves the substrate runs more than one image / one protocol.
#[test]
#[ignore = "LIVE-pulls redis:alpine from docker.io and boots it (network + heavy)"]
fn real_redis_image_boots_and_answers_ping_from_the_host() {
    use hologram_store_mem::MemKappaStore;
    use hologram_substrate_core::KappaStore;
    use holospaces::import::{parse_image_ref, pull_image};

    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let store = MemKappaStore::new();
    let image = parse_image_ref("redis:alpine").expect("parse redis:alpine");
    let img = pull_image(&store, &image, holospaces::Arch::X64).expect("live-pull redis:alpine (amd64)");

    let cfg_bytes = store.get(img.config()).unwrap().unwrap().as_ref().to_vec();
    let mut run = run_config_from_oci(&cfg_bytes).expect("redis image declares an Entrypoint/Cmd");
    // Disable protected mode so a client from the NAT gateway (non-loopback) is served (docker run redis --protected-mode no).
    run.argv.push("--protected-mode".into());
    run.argv.push("no".into());
    eprintln!("CC-60 G4: redis image's real argv = {:?}", run.argv);

    let layer_bytes: Vec<Vec<u8>> =
        img.layers().iter().map(|k| store.get(k).unwrap().unwrap().as_ref().to_vec()).collect();
    let media = img.layer_media_types();
    let layers: Vec<Layer> =
        layer_bytes.iter().zip(media.iter()).map(|(b, mt)| Layer { media_type: mt, blob: b }).collect();

    let shq = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    let argv_str = run.argv.iter().map(|a| shq(a)).collect::<Vec<_>>().join(" ");
    let script = format!(
        "ip addr add 10.0.2.15/24 dev eth0 2>/dev/null; ip link set eth0 up 2>/dev/null; \
         ip route add default via 10.0.2.2 2>/dev/null; echo HOLO-NET-UP; exec {argv_str}"
    );
    let cfg = RunConfig {
        argv: vec!["/bin/sh".into(), "-c".into(), script],
        env: run.env.clone(),
        workdir: if run.workdir.is_empty() { "/".into() } else { run.workdir.clone() },
        uid: 0,
        gid: 0,
        net_up: false,
    };
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).expect("compile image-init.c first");
    let init = image_init(&template, &cfg).expect("patch image-init with redis's entrypoint");
    let rootfs = assemble_ext4_bootable(&layers, &init, 768 * 1024 * 1024).expect("assemble redis rootfs");

    let mut ingress = StdIngress::new();
    let host_port = ingress.forward(0, 6379).expect("forward host → guest :6379");
    let mut cpu = Cpu::boot_linux_disk(1024 * 1024 * 1024, &kernel, rootfs, NIC_CMDLINE);
    cpu.attach_net_forward(Box::new(NoEgress), Box::new(ingress));

    let (tx, rx) = mpsc::channel::<String>();
    let mut client_started = false;
    for _ in 0..1200 {
        cpu.run(5_000_000);
        if !client_started && String::from_utf8_lossy(cpu.console()).contains("HOLO-NET-UP") {
            client_started = true;
            let tx = tx.clone();
            std::thread::spawn(move || {
                for _ in 0..120 {
                    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", host_port)) {
                        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = s.write_all(b"PING\r\n");
                        let mut resp = [0u8; 64];
                        if let Ok(n) = s.read(&mut resp) {
                            let text = String::from_utf8_lossy(&resp[..n]).into_owned();
                            if text.contains("PONG") {
                                let _ = tx.send(text);
                                return;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(400));
                }
            });
        }
        if let Ok(text) = rx.try_recv() {
            eprintln!("CC-60 G4: LIVE redis:alpine is REACHABLE from the host — PING → {:?} (real RESP).", text.trim());
            assert!(text.contains("PONG"));
            return;
        }
    }
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let tail: Vec<_> = con.lines().rev().take(20).collect::<Vec<_>>().into_iter().rev().collect();
    panic!("redis never answered PING (net_up={client_started}). boot tail (gap map):\n  {}", tail.join("\n  "));
}
