//! `CC-76` — the browser `open(κ)` path, at the core level: resume a warm `.holo` and render the guest's
//! live app over the IN-TAB loopback bridge (no host socket — a wasm tab has none).
//!
//! This is the exact sequence a browser tab runs to turn a κ-link into a live app: `X64Workspace::resume_kappa`
//! (rebuild the machine from the warm blob) → `enable_loopback` (in-tab bridge to a server INSIDE the guest)
//! → `dial_guest` + `guest_send`/`guest_recv` (make an HTTP request to the app and read its real response,
//! which the page renders). The wasm `X64Workspace` methods are 1:1 wrappers of the `x64::Cpu` methods
//! exercised here (CC-76 S1), so proving it on the core proves the tab's mechanism. It also proves the
//! virtio-net device survives the warm snapshot for the LOOPBACK path (CC-73 proved it for an external
//! socket; the browser uses loopback).

use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{address_bytes, KappaStore};
use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::net::NoEgress;
use holospaces::emulator::x64::Cpu;
use holospaces::image_init::{image_init, RunConfig};

fn art(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}
fn gunzip(p: PathBuf) -> Vec<u8> {
    use std::io::Read;
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
    // Serve a real, styled HTML page (with the BODY marker inside) so a browser tab RENDERS a web page,
    // not just text. Content-Length is computed in-guest so the response is well-formed.
    let html = format!(
        "<!doctype html><html><head><meta charset=utf-8><title>Hologram</title></head>\
         <body style=\"font-family:system-ui,sans-serif;background:#0b0e14;color:#cdd6f4;text-align:center;padding:8vh 6vw\">\
         <h1 style=\"color:#89b4fa;font-size:2.4rem\">Hello from a κ-snapshot</h1>\
         <p style=\"font-size:1.15rem;max-width:44rem;margin:1rem auto;line-height:1.6\">This page is served by a real Linux \
         server that was booted <b>once</b>, frozen into a content-addressed <b>κ</b>, and <b>resumed live inside your \
         browser tab</b> — no boot, no app server, 100% serverless.</p>\
         <code style=\"color:#a6e3a1\">{BODY}</code></body></html>"
    );
    let script = format!(
        "/bin/busybox ip link set eth0 up\n\
         /bin/busybox ip addr add 10.0.2.15/24 dev eth0\n\
         /bin/busybox ip route add default via 10.0.2.2 2>/dev/null\n\
         BODY='{html}'\n\
         LEN=$(printf '%s' \"$BODY\" | /bin/busybox wc -c)\n\
         printf 'HTTP/1.0 200 OK\\r\\nContent-Type: text/html\\r\\nContent-Length: %s\\r\\n\\r\\n%s' \"$LEN\" \"$BODY\" > /resp\n\
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

#[test]
#[ignore = "boots a NIC'd server, warm-snapshots it, and renders it over the in-tab loopback bridge (heavy)"]
fn a_warm_holo_renders_its_app_over_the_loopback_bridge() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));

    // ── boot the server with a NIC until listening, then warm-snapshot it (the `holo run` cached .holo). ──
    let mut ingress = holospaces::emulator::net::StdIngress::new();
    let _ = ingress.forward(0, 8080).expect("bind a forwarded host port");
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
    assert!(listening, "server never listened before snapshot");
    for _ in 0..30 {
        cpu.run(5_000_000); // drain so the snapshot captures an idle-listening server
    }
    let blob = cpu.snapshot_kappa_blob();
    drop(cpu);

    // ── the "tab": resume the warm .holo, enable the in-tab loopback bridge, dial the guest app, render. ──
    // (Exactly what `open.html`'s worker does via `X64Workspace::{resume_kappa, enable_loopback, dial_guest,
    //  guest_send, guest_recv}` — the wasm methods added in CC-76 S1 are 1:1 wrappers of these.)
    let mut tab = Cpu::new(0x1000);
    assert!(tab.restore_kappa_blob(&blob), "resume the warm .holo (X64Workspace::resume_kappa)");
    assert!(tab.enable_loopback(), "enable the in-tab loopback bridge on the resumed device");
    // Give the resumed guest a moment to schedule (its nc server is blocked in accept()).
    for _ in 0..10 {
        tab.run(5_000_000);
    }
    let id = tab.dial_guest(8080).expect("dial the guest's server on 8080 over the loopback bridge");
    tab.guest_send(id, b"GET / HTTP/1.0\r\nHost: app\r\n\r\n");
    let mut rendered = Vec::new();
    for _ in 0..300 {
        tab.run(2_000_000);
        rendered.extend_from_slice(&tab.guest_recv(id));
        if rendered.windows(BODY.len()).any(|w| w == BODY.as_bytes()) {
            break;
        }
    }
    let text = String::from_utf8_lossy(&rendered).into_owned();
    assert!(
        text.contains(BODY),
        "the tab never rendered the guest app's response over the loopback bridge. got: {text:?}"
    );
    eprintln!(
        "CC-76: a warm .holo RESUMED in a tab renders its live app over the loopback bridge — got {BODY:?} \
         (no host socket). This is `open(κ)` at the core level."
    );
}

/// Generate the browser `open(κ)` fixture: a NIC'd server warm-snapshotted to a `.holo` that
/// `open.html` resumes + renders over the loopback bridge. The NIC (virtio-net) MUST be attached at
/// snapshot time so `enable_loopback` finds a device on resume. Run:
/// `cargo test -p holospaces --release --features net --test cc76_browser_loopback_resume
///  generate_browser_server_fixture -- --ignored --nocapture`
#[test]
#[ignore = "writes crates/holospaces-web/web/fixtures/x64-server-loopback.holo (slow, one-time)"]
fn generate_browser_server_fixture() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let mut ingress = holospaces::emulator::net::StdIngress::new();
    let _ = ingress.forward(0, 8080).expect("bind a forwarded host port");
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
    assert!(listening, "server never listened before snapshot");
    for _ in 0..30 {
        cpu.run(5_000_000);
    }
    let blob = cpu.snapshot_kappa_blob();
    let out = art("crates/holospaces-web/web/fixtures/x64-server-loopback.holo");
    std::fs::create_dir_all(out.parent().unwrap()).ok();
    std::fs::write(&out, &blob).expect("write browser server fixture");
    eprintln!("WROTE {} ({} MiB) — resume + enable_loopback + dial 8080 renders {BODY:?}", out.display(), blob.len() / (1024 * 1024));

    // Also publish it as a CONTENT-ADDRESSED κ-store for the real `open(κ)` path (Polish-1): the manifest
    // + each UNIQUE page written under `store/<κ>` (":" → "_" so it's a valid filename), and the manifest's
    // OWN κ printed — that κ is the whole share handle. The browser fetches store/<κ> by κ and L5-verifies
    // every page in wasm (`resume_kappa_streamed`), so a tampered page is refused. Only unique pages exist.
    let store = MemKappaStore::new();
    let snap = cpu.snapshot_kappa(&store).expect("κ-seal the running server");
    let manifest = snap.to_manifest_bytes();
    let store_dir = art("crates/holospaces-web/web/fixtures/store");
    std::fs::create_dir_all(&store_dir).ok();
    let safe = |k: &str| k.replace(':', "_");
    let manifest_kappa = address_bytes(&manifest).as_str().to_owned();
    std::fs::write(store_dir.join(safe(&manifest_kappa)), &manifest).expect("write manifest by κ");
    // The share handle (the manifest's κ) — the browser witness reads this to build `?k=<κ>`.
    std::fs::write(store_dir.join(".manifest-kappa"), &manifest_kappa).expect("write manifest-κ handle");
    let mut pages = std::collections::BTreeSet::new();
    for k in snap.page_kappas() {
        let ks = k.as_str().to_owned();
        if pages.insert(ks.clone()) {
            let bytes = store.get(k).unwrap().unwrap().as_ref().to_vec();
            std::fs::write(store_dir.join(safe(&ks)), &bytes).expect("write page by κ");
        }
    }
    eprintln!(
        "WROTE κ-store {} ({} unique pages) — open(κ) handle:\n  MANIFEST-κ = {manifest_kappa}",
        store_dir.display(),
        pages.len()
    );
}
