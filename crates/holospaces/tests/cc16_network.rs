//! `CC-16` — the running OS reaches the open internet through holospaces.
//!
//! A devcontainer is not a dev environment if it can't `git clone`, `apt-get`,
//! or `npm install` from the internet. There is no raw NIC behind a browser tab,
//! so the guest OS drives a real `virtio-net` device whose frames are terminated
//! by a **userspace TCP/IP NAT** (ARP + DHCP + the guest-facing TCP state
//! machine) and whose TCP streams are carried out over a pluggable **egress**
//! transport (ADR-014). This witness boots a real Linux kernel on the emulator,
//! lets it autoconfigure its interface with DHCP against the NAT, and has its
//! userspace open a TCP connection and complete an HTTP exchange — out through
//! the native egress (a host socket) to a real listening server.
//!
//! The differential oracle is `qemu-system-riscv64`'s own user-mode (slirp)
//! network: the same kernel + init boots there and produces the same markers.
//! The NAT reproduces slirp's addressing (`10.0.2.0/24`, gateway `10.0.2.2`) and
//! its `guestfwd` port-forward, so the guest software is byte-for-byte identical.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4, Layer};
use holospaces::emulator::net::StdEgress;
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};

fn cc16_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc16")
}

fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(cc16_dir().join("image/blobs/sha256").join(hex)).ok()
}

fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(cc16_dir().join("image/oci-layout")).unwrap();
    let index = std::fs::read(cc16_dir().join("image/index.json")).unwrap();
    ingest_image(
        store,
        &layout,
        &index,
        holospaces::Arch::Riscv64,
        blob_bytes,
    )
}

fn gunzip(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out).unwrap();
    out
}

/// The OS boots, does DHCP over `virtio-net`, opens a TCP connection through the
/// userspace NAT, and completes an HTTP exchange with a real host server reached
/// over the native egress. Heavy (a real-OS boot), so `#[ignore]`d.
#[test]
#[ignore]
fn the_os_reaches_the_internet_through_the_userspace_nat() {
    // A real host server the guest's HTTP request must reach (the "internet").
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a host server");
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf); // the guest's request line
            let _ = sock
                .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 22\r\n\r\nHELLO-FROM-HOST-SERVER");
            let _ = sock.flush();
        }
    });

    // Assemble the init rootfs from its OCI image.
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the CC-16 image");
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

    // The guest dials 10.0.2.9:8080 (as the QEMU oracle's `guestfwd` does); the
    // native egress port-forwards that to the host server on 127.0.0.1:`port`.
    let egress = StdEgress::new().redirect([10, 0, 2, 9], 8080, "127.0.0.1", port);
    let kernel = gunzip(&cc16_dir().join("kernel/Image.gz"));
    let mut emu = MachineSpec::devcontainer_net()
        .boot_net(&kernel, rootfs, Box::new(egress))
        .expect("boot with networking");
    emu.run(800_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();
    let _ = server.join();

    assert!(
        console.contains("IP-Config: Complete") || console.contains("NET-CONNECTED"),
        "the OS brought up its network interface (DHCP IP-Config or the NAT-connected marker); console:\n{console}"
    );
    assert!(
        console.contains("NET-CONNECTED"),
        "the OS opened a TCP connection through the NAT; console:\n{console}"
    );
    assert!(
        console.contains("HELLO-FROM-HOST-SERVER"),
        "the OS received the host server's HTTP response through the egress (CC-16); console:\n{console}"
    );
    assert!(
        console.contains("NET-DONE"),
        "the network exchange completed; console:\n{console}"
    );
}
