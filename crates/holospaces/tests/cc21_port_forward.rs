//! `CC-21` — a server running in the devcontainer is reachable as a forwarded
//! port (the running-app preview).
//!
//! A Codespace/Gitpod forwards a port so the web app you run in the environment
//! is reachable from outside. holospaces does this with the **ingress dual** of
//! the `CC-16` egress: the userspace TCP/IP NAT accepts an *inbound* connection
//! on a forwarded port and opens a connection *to* the server inside the
//! devcontainer (the NAT is the active opener toward the guest). This witness
//! boots a real Linux running a TCP server, forwards a host port to it, and a
//! host client reaches the guest server through the forward and reads its HTTP
//! response.
//!
//! The differential oracle is `qemu-system-riscv64`'s user-mode `hostfwd` (the
//! same `10.0.2.0/24` NAT model); the guest software is byte-for-byte identical.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4, Layer};
use holospaces::emulator::net::{StdEgress, StdIngress};
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};

fn cc21_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc21")
}
fn cc16_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc16")
}

fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(cc21_dir().join("image/blobs/sha256").join(hex)).ok()
}
fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(cc21_dir().join("image/oci-layout")).unwrap();
    let index = std::fs::read(cc21_dir().join("image/index.json")).unwrap();
    ingest_image(store, &layout, &index, blob_bytes)
}
fn gunzip(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out).unwrap();
    out
}

/// A server in the devcontainer is reached through a forwarded port. Heavy (a
/// real-OS boot), so `#[ignore]`d.
#[test]
#[ignore]
fn a_server_in_the_devcontainer_is_reachable_through_a_forwarded_port() {
    // Forward an ephemeral host port to the guest's listening port 8080.
    let mut ingress = StdIngress::new();
    let host_port = ingress
        .forward(0, 8080)
        .expect("bind a forwarded host port");

    // Assemble the server-init rootfs from its OCI image.
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the CC-21 image");
    let blobs: Vec<(String, Vec<u8>)> = img
        .layers()
        .iter()
        .zip(img.layer_media_types())
        .map(|(k, mt)| (mt.clone(), store.get(k).unwrap().unwrap().as_ref().to_vec()))
        .collect();
    let layers: Vec<Layer> = blobs
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();
    let rootfs = assemble_ext4(&layers).expect("assemble rootfs");
    let kernel = gunzip(&cc16_dir().join("kernel/Image.gz"));

    let mut emu = MachineSpec::devcontainer_net()
        .boot_net_forward(
            &kernel,
            rootfs,
            Box::new(StdEgress::new()),
            Box::new(ingress),
        )
        .expect("boot with port forwarding");

    // Run in chunks; once the guest server is listening, a host client connects
    // through the forward (a separate thread — it reaches the host listener while
    // the emulator services the NAT). The client retries to ride out boot timing.
    let (tx, rx) = mpsc::channel::<String>();
    let mut client_started = false;
    let mut served = false;
    for _ in 0..400 {
        if !matches!(emu.run(5_000_000), holospaces::emulator::Halt::OutOfBudget) {
            break;
        }
        let console = String::from_utf8_lossy(emu.console());
        if !client_started && console.contains("SERVER-LISTENING") {
            client_started = true;
            let tx = tx.clone();
            std::thread::spawn(move || {
                for _ in 0..40 {
                    if let Ok(mut s) = TcpStream::connect(("127.0.0.1", host_port)) {
                        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                        let _ = s.write_all(b"GET / HTTP/1.0\r\nHost: app\r\n\r\n");
                        // Accumulate until the marker / EOF / read timeout (the
                        // one-shot server may close without our FIN being acked).
                        let mut resp = Vec::new();
                        let mut chunk = [0u8; 512];
                        loop {
                            match s.read(&mut chunk) {
                                Ok(0) => break,
                                Ok(n) => {
                                    resp.extend_from_slice(&chunk[..n]);
                                    if resp.windows(23).any(|w| w == b"HELLO-FROM-GUEST-SERVER") {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let text = String::from_utf8_lossy(&resp).into_owned();
                        if text.contains("HELLO-FROM-GUEST-SERVER") {
                            let _ = tx.send(text);
                            return;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(300));
                }
            });
        }
        if let Ok(resp) = rx.try_recv() {
            assert!(resp.contains("HELLO-FROM-GUEST-SERVER"));
            served = true;
            break;
        }
    }

    // Collect the client's result (it arrives around the guest's close/reboot).
    if !served {
        if let Ok(resp) = rx.recv_timeout(Duration::from_secs(8)) {
            served = resp.contains("HELLO-FROM-GUEST-SERVER");
        }
    }

    let console = String::from_utf8_lossy(emu.console()).into_owned();
    assert!(
        console.contains("SERVER-LISTENING"),
        "the devcontainer's server bound and listened; console:\n{console}"
    );
    assert!(
        served,
        "a host client reached the guest server through the forwarded port (CC-21); console:\n{console}"
    );
}

/// The config's `forwardPorts` is honoured end-to-end: parsing it yields a real
/// bound forward per declared port (a host listener bridged to the guest port),
/// never a parsed-and-dropped field. Light (no OS boot) — it witnesses the
/// config → ingress wiring [`DevContainer::port_forwards`] establishes.
#[test]
fn declared_forward_ports_are_honoured_from_the_config() {
    use holospaces::boot::devcontainer;

    let cfg = br#"{"image":"busybox","forwardPorts":[8080, 9090]}"#;
    let dc = devcontainer::parse(cfg).expect("the config parses");
    assert_eq!(
        dc.forward_ports,
        vec![8080, 9090],
        "both declared ports are parsed"
    );

    let (ingress, bound) = dc.port_forwards().expect("the forwards bind");
    assert_eq!(
        bound.len(),
        2,
        "every declared forwardPorts entry is honoured (none dropped)"
    );
    assert_eq!(
        bound.iter().map(|(_, g)| *g).collect::<Vec<_>>(),
        vec![8080, 9090],
        "each forward targets its declared guest port"
    );
    // Each forward is a *real* bound host listener: a connection to the host port
    // is accepted (the forward exists even before the guest server is up).
    for (host_port, _guest) in &bound {
        TcpStream::connect(("127.0.0.1", *host_port))
            .expect("the forwarded host port is a live listener");
    }
    drop(ingress);
}
