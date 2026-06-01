//! `CC-17` (Phase 2 foundation) — the editor-side filesystem over the shared
//! workspace.
//!
//! The real VS Code web workbench (Phase 1) edits the holospace's files through a
//! `FileSystemProvider`. Per ADR-012/015 that provider is *not* a separate store —
//! it is the **`virtio-9p` workspace of `CC-15`**: the κ-addressed content the
//! editor and the running OS *share* (Law L1). This witness exercises holospaces'
//! editor-side API over that share — list, write, and read — and asserts the
//! content is content-addressed (a write's identity is its κ, Law L1/L2) and that
//! a file the editor writes is the *same content* the guest OS reads over
//! `virtio-9p` (one content; the workbench `FileSystemProvider` binds to this over
//! the wasm peer). The browser wiring (a service worker bridging the workbench's
//! web-extension provider to the wasm peer) sits on top of this substrate API.
//!
//! The protocol authority remains 9P2000.L (`CC-15`); the differential oracle for
//! the OS-visible sharing is `qemu-system-riscv64`'s own 9p server.

use std::io::Read;
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4, Layer};
use holospaces::emulator::Emulator;
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};
use holospaces::realizations::address;

fn cc15_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc15")
}
fn cc14_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14")
}

/// The editor-side filesystem is content-addressed and *is* the shared workspace:
/// holospaces writes a file, lists it, reads it back byte-identically, and the
/// file's identity is the κ of its content (Law L1/L2). Fast — no boot.
#[test]
fn the_editor_lists_writes_and_reads_the_shared_workspace_by_kappa() {
    let mut emu = Emulator::new(0x8000_0000, 1 << 20);
    // Attach a workspace holospaces seeds (the editor's initial tree).
    emu.attach_workspace(&[("README.md", b"# holospace\n")]);

    // The editor enumerates the workspace (FileSystemProvider.readDirectory).
    let listing = emu.workspace_list();
    assert!(
        listing.iter().any(|(n, dir, _)| n == "README.md" && !dir),
        "the editor sees the seeded file; listing: {listing:?}"
    );

    // The editor saves a new file (FileSystemProvider.writeFile) into the share.
    let content = b"fn main() { println!(\"hello from the holospace\"); }\n";
    emu.workspace_write("main.rs", content);

    // It reads back byte-identically (FileSystemProvider.readFile).
    assert_eq!(
        emu.workspace_file("main.rs"),
        Some(&content[..]),
        "the editor reads back exactly what it wrote"
    );
    // The file's identity is the κ of its content — content addressing (Law L1/L2).
    let k1 = address(content);
    let k2 = address(emu.workspace_file("main.rs").unwrap());
    assert_eq!(k1, k2, "the workspace file is identified by its content κ");

    // A re-write updates in place (no duplicate entries) and advances the κ (L1).
    emu.workspace_write("main.rs", b"fn main() {}\n");
    let count = emu
        .workspace_list()
        .iter()
        .filter(|(n, _, _)| n == "main.rs")
        .count();
    assert_eq!(count, 1, "re-writing updates in place, not duplicates");
    assert_ne!(
        address(emu.workspace_file("main.rs").unwrap()),
        k1,
        "an edit advances the file's κ (content identity, Law L1)"
    );
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

/// The file the *editor* writes is the *same content* the running OS reads over
/// `virtio-9p` — one shared workspace (Law L1). Heavy (a real-OS boot), so
/// `#[ignore]`d; reuses the `CC-15` init, which reads `from-holospaces.txt`.
#[test]
#[ignore]
fn a_file_the_editor_writes_is_read_by_the_os_over_9p() {
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
    let kernel = gunzip(&cc14_dir().join("kernel/Image.gz"));

    // The editor writes the file the CC-15 init reads — through the same
    // workspace API the workbench's FileSystemProvider uses. No seed: the content
    // exists only because the editor wrote it.
    let mut emu = MachineSpec::devcontainer()
        .boot_workspace(&kernel, rootfs, &[])
        .expect("boot with the workspace share");
    emu.workspace_write("from-holospaces.txt", b"from-holospaces-9p-share-OK\n");
    emu.run(600_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();

    assert!(
        console.contains("READ:from-holospaces-9p-share-OK"),
        "the OS read the file the EDITOR wrote into the shared workspace (L1); console:\n{console}"
    );
}
