//! `CC-15` — the editor and the running OS share the workspace filesystem
//! (arc42 ch.10; ADR-011). holospaces serves the workspace over a spec-conformant
//! `virtio-9p` device (the 9P2000.L protocol); the guest OS mounts it over
//! `virtio-9p` and reads/writes the *same* files holospaces holds — a file
//! holospaces places on the share is read by the OS, and a file the OS writes is
//! read back by holospaces (one content, Law L1).
//!
//! The protocol authority is **9P2000.L**; the differential oracle is
//! `qemu-system-riscv64`'s own 9p server, which boots the same kernel + init and
//! produces the same markers.

use std::io::Read;
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4, Layer};
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};

fn cc15_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc15")
}
fn cc14_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14")
}

fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(cc15_dir().join("image/blobs/sha256").join(hex)).ok()
}

fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(cc15_dir().join("image/oci-layout")).unwrap();
    let index = std::fs::read(cc15_dir().join("image/index.json")).unwrap();
    ingest_image(store, &layout, &index, blob_bytes)
}

fn gunzip(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out).unwrap();
    out
}

/// The OS mounts the holospaces-served workspace over virtio-9p and reads + writes
/// the same files holospaces holds. Heavy (a real-OS boot), so `#[ignore]`d.
#[test]
#[ignore]
fn the_os_and_holospaces_share_the_workspace_over_virtio_9p() {
    // Assemble the 9p-init rootfs from its OCI image.
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the CC-15 image");
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

    // Boot with the root disk AND a shared workspace seeded by holospaces.
    let kernel = gunzip(&cc14_dir().join("kernel/Image.gz"));
    let seed: &[(&str, &[u8])] = &[("from-holospaces.txt", b"from-holospaces-9p-share-OK\n")];
    let mut emu = MachineSpec::devcontainer()
        .boot_workspace(&kernel, rootfs, seed)
        .expect("boot with the workspace share");
    emu.run(600_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();

    assert!(
        console.contains("9P-MOUNTED"),
        "the OS mounted the workspace over virtio-9p; console:\n{console}"
    );
    assert!(
        console.contains("READ:from-holospaces-9p-share-OK"),
        "the OS read the file holospaces placed on the share (L1); console:\n{console}"
    );
    // holospaces reads back the file the OS wrote over 9P — one shared content.
    let written = emu.workspace_file("from-guest.txt");
    assert_eq!(
        written,
        Some(&b"GUEST-WROTE-THIS\n"[..]),
        "holospaces reads back the file the OS wrote over 9P (one content, L1)"
    );
}
