//! `CC-33` — a server inside the booted devcontainer is reachable from the
//! workbench over the **in-process substrate bridge** (ADR-020).
//!
//! The browser peer's workbench extension host and the system emulator are one
//! process, so reaching a server *inside* the guest is not a network round trip —
//! it is an in-process ingress connection into the emulator's own userspace
//! TCP/IP NAT (`CC-16`), the inward dual of the egress relay and the same NAT the
//! native forwarded-port ingress (`CC-21`) drives. This is the transport the VS
//! Code remote extension host runs over (ADR-015/ADR-020): the workbench dials a
//! guest listener (`Emulator::dial_guest`), writes the client's bytes
//! (`guest_send`), and reads the guest's reply (`guest_recv`) — no relay, no
//! socket, no server.
//!
//! The differential oracle is the same `10.0.2.0/24` userspace NAT model as
//! `qemu-system-riscv64`'s user networking; the guest software is the very
//! `CC-21` image (a real TCP server on port 8080), reached here over the loopback
//! bridge instead of a host listener — the guest is byte-for-byte identical.

use std::io::Read;
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4, Layer};
use holospaces::emulator::net::NoEgress;
use holospaces::emulator::Halt;
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

/// A server in the devcontainer is reached over the in-process bridge. Heavy (a
/// real-OS boot), so `#[ignore]`d — run by the `CC-33` vv suite.
#[test]
#[ignore]
fn a_guest_server_is_reachable_over_the_in_process_substrate_bridge() {
    // Assemble the CC-21 server-init rootfs (a guest TCP server on port 8080).
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

    // Boot the networked devcontainer with the in-process loopback bridge: a
    // no-op egress (the guest only listens) + the loopback ingress. The guest
    // still gets its DHCP lease and a real TCP stack (those are the NAT's).
    let mut emu = MachineSpec::devcontainer_net()
        .boot_net(&kernel, rootfs, Box::new(NoEgress))
        .expect("boot the networked devcontainer");
    assert!(
        emu.enable_loopback(),
        "the loopback bridge attaches to the network device"
    );

    // Run until the guest server is listening.
    let mut listening = false;
    for _ in 0..400 {
        if !matches!(emu.run(5_000_000), Halt::OutOfBudget) {
            break;
        }
        if String::from_utf8_lossy(emu.console()).contains("SERVER-LISTENING") {
            listening = true;
            break;
        }
    }
    let console = String::from_utf8_lossy(emu.console()).into_owned();
    assert!(
        listening,
        "the devcontainer's server bound and listened; console:\n{console}"
    );

    // Dial the guest server over the in-process bridge — no host socket, no
    // thread; the workbench and the emulator are one process.
    let id = emu
        .dial_guest(8080)
        .expect("the loopback bridge is enabled, so dialing returns a connection id");
    // Pump so the NAT opens the connection toward the guest (SYN / SYN-ACK).
    for _ in 0..20 {
        emu.run(2_000_000);
    }
    // Host → guest: the request bytes.
    let request = b"GET / HTTP/1.0\r\nHost: app\r\n\r\n";
    emu.guest_send(id, request);

    // Guest → host: drain the reply until the server's marker arrives.
    let mut resp: Vec<u8> = Vec::new();
    for _ in 0..400 {
        emu.run(2_000_000);
        resp.extend(emu.guest_recv(id));
        if resp.windows(23).any(|w| w == b"HELLO-FROM-GUEST-SERVER") {
            break;
        }
        if !emu.guest_is_open(id) && resp.windows(23).any(|w| w == b"HELLO-FROM-GUEST-SERVER") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&resp).into_owned();
    assert!(
        text.contains("HELLO-FROM-GUEST-SERVER"),
        "the host reached the guest server over the in-process substrate bridge and read its \
         reply byte-faithfully — the request (host→guest) was delivered and the response \
         (guest→host) returned (CC-33); got:\n{text:?}\nconsole:\n{console}"
    );
    emu.guest_close(id);
}
