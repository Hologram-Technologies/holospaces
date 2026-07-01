//! `CC-69` — the real-image matrix: drive a DIVERSE fleet of live Docker images through the proven
//! pull→boot→reach pipeline and record, per image, PASS (app reachable / bytes byte-exact) or a PRECISE
//! gap (console tail + guest exception trace). This is the authoritative "what the κ substrate runs today"
//! map + the ranked list of what to fix next. Each live image is heavy → `#[ignore]`; run explicitly.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

struct Spec {
    image: &'static str,
    port: u16,
    /// `None` = use the image's own Entrypoint/Cmd; `Some` = override (a runtime image like python/node).
    cmd_override: Option<Vec<&'static str>>,
    probe: &'static [u8],
    expect: &'static str,
    /// A no-shell image (distroless/scratch): the init brings eth0 up itself (net_up) and DIRECT-execs the
    /// entrypoint — no `/bin/sh` prelude.
    no_shell: bool,
}

/// Pull → boot the image's app with a NIC → probe from the host. Ok(report) on reach; Err(gap) otherwise.
fn drive(spec: &Spec) -> Result<String, String> {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).map_err(|e| format!("template: {e}"))?;
    let store = MemKappaStore::new();
    let iref = parse_image_ref(spec.image).map_err(|e| format!("ref: {e:?}"))?;
    let img = pull_image(&store, &iref, holospaces::Arch::X64).map_err(|e| format!("pull: {e:?}"))?;
    let cfg_bytes = store.get(img.config()).unwrap().unwrap().as_ref().to_vec();
    let rc = run_config_from_oci(&cfg_bytes).unwrap_or_default();

    let argv: Vec<String> = match &spec.cmd_override {
        Some(v) => v.iter().map(|s| s.to_string()).collect(),
        None => rc.argv.clone(),
    };
    if argv.is_empty() {
        return Err("image declares no Entrypoint/Cmd and no override given".into());
    }
    let cfg = if spec.no_shell {
        // Distroless/scratch: the init brings eth0 up itself, then DIRECT-execs the entrypoint (no shell).
        RunConfig {
            argv,
            env: rc.env.clone(),
            workdir: if rc.workdir.is_empty() { "/".into() } else { rc.workdir.clone() },
            uid: 0,
            gid: 0,
            net_up: true,
        }
    } else {
        // Shell image: a prelude brings eth0 up, then execs the entrypoint.
        let shq = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let argv_str = argv.iter().map(|a| shq(a)).collect::<Vec<_>>().join(" ");
        let script = format!(
            "ip addr add 10.0.2.15/24 dev eth0 2>/dev/null; ip link set eth0 up 2>/dev/null; \
             ip route add default via 10.0.2.2 2>/dev/null; echo HOLO-NET-UP; exec {argv_str}"
        );
        RunConfig {
            argv: vec!["/bin/sh".into(), "-c".into(), script],
            env: rc.env.clone(),
            workdir: if rc.workdir.is_empty() { "/".into() } else { rc.workdir.clone() },
            uid: 0,
            gid: 0,
            net_up: false,
        }
    };
    let init = image_init(&template, &cfg).ok_or("run-config too large for init table")?;
    let rootfs = assemble_ext4_bootable(
        &img.layers().iter().map(|k| store.get(k).unwrap().unwrap().as_ref().to_vec()).collect::<Vec<_>>()
            .iter()
            .zip(img.layer_media_types().iter())
            .map(|(b, mt)| Layer { media_type: mt, blob: b })
            .collect::<Vec<_>>(),
        &init,
        768 * 1024 * 1024,
    )
    .map_err(|e| format!("assemble: {e:?}"))?;

    let mut ingress = StdIngress::new();
    let host_port = ingress.forward(0, spec.port).map_err(|e| format!("forward: {e}"))?;
    let mut cpu = Cpu::boot_linux_disk(1024 * 1024 * 1024, &kernel, rootfs, NIC_CMDLINE);
    cpu.attach_net_forward(Box::new(NoEgress), Box::new(ingress));
    let _ = drain_exc_trace(); // clear pre-boot noise

    let (tx, rx) = mpsc::channel::<String>();
    let (probe, expect) = (spec.probe.to_vec(), spec.expect.to_string());
    let mut started = false;
    let t0 = Instant::now();
    for _ in 0..1400 {
        cpu.run(5_000_000);
        if !started && String::from_utf8_lossy(cpu.console()).contains("HOLO-NET-UP") {
            started = true;
            let (tx, probe, expect) = (tx.clone(), probe.clone(), expect.clone());
            std::thread::spawn(move || {
                for _ in 0..120 {
                    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", host_port)) {
                        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = s.write_all(&probe);
                        let mut resp = Vec::new();
                        let mut c = [0u8; 1024];
                        loop {
                            match s.read(&mut c) {
                                Ok(0) => break,
                                Ok(n) => {
                                    resp.extend_from_slice(&c[..n]);
                                    if resp.windows(expect.len()).any(|w| w == expect.as_bytes()) {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let text = String::from_utf8_lossy(&resp).into_owned();
                        if text.contains(&expect) {
                            let _ = tx.send(text);
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(400));
                }
            });
        }
        if let Ok(text) = rx.try_recv() {
            return Ok(format!("insns={} wall={:?} first={:?}", cpu.insns(), t0.elapsed(), text.lines().next().unwrap_or("")));
        }
    }
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let tail: Vec<_> = con.lines().rev().take(14).collect::<Vec<_>>().into_iter().rev().collect();
    let exc = drain_exc_trace();
    let exc_tail: Vec<_> = exc.iter().rev().take(6).cloned().collect();
    Err(format!(
        "net_up={started}; last exceptions: {exc_tail:?}\n  console tail:\n  {}",
        tail.join("\n  ")
    ))
}

fn report(name: &str, spec: &Spec) {
    match drive(spec) {
        Ok(r) => eprintln!("CC-69 [{name}] {} → PASS  {r}", spec.image),
        Err(g) => eprintln!("CC-69 [{name}] {} → GAP\n  {g}", spec.image),
    }
}

#[test]
#[ignore]
fn img_httpd_alpine() {
    report("httpd-musl", &Spec {
        image: "httpd:alpine",
        port: 80,
        cmd_override: None,
        probe: b"GET / HTTP/1.0\r\nHost: x\r\n\r\n",
        expect: "200",
        no_shell: false,
    });
}

#[test]
#[ignore]
fn img_python_httpserver() {
    report("python-interp", &Spec {
        image: "python:3-alpine",
        port: 8080,
        cmd_override: Some(vec!["python3", "-m", "http.server", "8080", "--bind", "0.0.0.0"]),
        probe: b"GET / HTTP/1.0\r\nHost: x\r\n\r\n",
        expect: "200",
        no_shell: false,
    });
}

#[test]
#[ignore]
fn img_httpd_debian_glibc() {
    report("httpd-glibc", &Spec {
        image: "httpd:latest",
        port: 80,
        cmd_override: None,
        probe: b"GET / HTTP/1.0\r\nHost: x\r\n\r\n",
        expect: "200",
        no_shell: false,
    });
}

#[test]
#[ignore]
fn img_netup_in_init() {
    // Validate net-up-in-init (CC-69 Step 1): NO shell prelude — the freestanding init itself brings eth0 up
    // via ioctl, then DIRECT-execs the image's entrypoint. (nginx:alpine pulls cleanly; a true scratch static
    // image awaits the Docker-schema2 media-type gap below.) Reachability proves the ioctl net-up works.
    report("netup-in-init", &Spec {
        image: "nginx:alpine",
        port: 80,
        cmd_override: None,
        probe: b"GET / HTTP/1.0\r\nHost: x\r\n\r\n",
        expect: "200",
        no_shell: true,
    });
}

#[test]
#[ignore]
fn img_traefik_whoami_scratch() {
    // A true scratch/no-shell static Go server. If it pulls (OCI media types), net-up-in-init serves it with
    // no /bin/sh anywhere. If the pull is refused, that's the Docker-schema2 media-type coverage gap.
    report("scratch-static", &Spec {
        image: "traefik/whoami",
        port: 80,
        cmd_override: None,
        probe: b"GET / HTTP/1.0\r\nHost: x\r\n\r\n",
        expect: "200",
        no_shell: true,
    });
}

#[test]
#[ignore]
fn img_distroless_http_echo() {
    // The exact Docker-schema2 scratch image that failed the pull in CC-69 — now unblocked by the
    // media-type widening (CC-70) + reachable via net-up-in-init (no /bin/sh).
    report("distroless-http-echo", &Spec {
        image: "hashicorp/http-echo",
        port: 8080,
        cmd_override: Some(vec!["/http-echo", "-listen=:8080", "-text=HOLO-DISTROLESS-OK"]),
        probe: b"GET / HTTP/1.0\r\nHost: x\r\n\r\n",
        expect: "HOLO-DISTROLESS-OK",
        no_shell: true,
    });
}
