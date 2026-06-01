//! `CC-14` — the devcontainer boots a real OS root filesystem over a VirtIO
//! block device (arc42 ch.10 Conformance catalog; ADR-011).
//!
//! This is the **end-to-end keystone**: it exercises the whole devcontainer data
//! path the gap analysis identified, with no mocks —
//!
//! 1. a real **OCI image** (`vv/artifacts/cc14/image`, a `tar+gzip` layer) is
//!    ingested and verified by re-derivation (`CC-10`);
//! 2. the **Layer Assembler** turns its layers into a real `ext4` root
//!    filesystem (the in-crate gzip+tar+overlay+ext4 writer; Law L4);
//! 3. that filesystem is the backing of the emulator's **`virtio-blk`** device
//!    over the **`virtio-mmio`** transport, its interrupt routed by the **PLIC**
//!    (OASIS VirtIO v1.2; RISC-V PLIC spec);
//! 4. a real, unmodified **Linux** kernel (`vv/artifacts/cc14/kernel`, built with
//!    `VIRTIO_MMIO`+`VIRTIO_BLK`+`EXT4`) boots on the emulator, **mounts the
//!    rootfs over `/dev/vda`**, and runs its real userspace `/init`.
//!
//! The external authority / differential oracle is `qemu-system-riscv64`: the
//! same kernel + assembled rootfs prints the same userspace marker there
//! (`vv/artifacts/cc14/expected.txt`); behaviour matches QEMU.

use std::io::Read;
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4, Layer};
use holospaces::emulator::{Emulator, Halt};
use holospaces::oci::{ingest_image, IngestedImage, OciError};

fn cc14_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14")
}

fn image_dir() -> PathBuf {
    cc14_dir().join("image")
}

fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(image_dir().join("blobs/sha256").join(hex)).ok()
}

fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(image_dir().join("oci-layout")).expect("oci-layout");
    let index = std::fs::read(image_dir().join("index.json")).expect("index.json");
    ingest_image(store, &layout, &index, blob_bytes)
}

fn gunzip(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).expect("read gz");
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out).expect("gunzip");
    out
}

/// Assemble the rootfs from the ingested OCI image (CC-10 → Layer Assembler).
fn assemble_rootfs(store: &MemKappaStore, img: &IngestedImage) -> Vec<u8> {
    let blobs: Vec<(String, Vec<u8>)> = img
        .layers()
        .iter()
        .zip(img.layer_media_types())
        .map(|(k, mt)| {
            (
                mt.clone(),
                store.get(k).unwrap().expect("layer").as_ref().to_vec(),
            )
        })
        .collect();
    let layers: Vec<Layer> = blobs
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();
    assemble_ext4(&layers).expect("assemble rootfs")
}

/// The full chain — ingest, assemble, and boot a real Linux that mounts the
/// assembled ext4 over the emulator's virtio-blk and runs userspace. Heavy
/// (a real kernel boot on the interpreter), so `#[ignore]` like the other
/// real-OS-boot witnesses; run with `--ignored --release`.
#[test]
#[ignore]
fn a_real_linux_mounts_an_assembled_oci_rootfs_over_virtio_blk() {
    // 1 + 2: ingest the OCI image and assemble its layers into an ext4 rootfs.
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the OCI image");
    let rootfs = assemble_rootfs(&store, &img);
    assert!(
        rootfs.len() >= 4096 && rootfs.len().is_multiple_of(4096),
        "assembled a whole-block ext4 rootfs ({} bytes)",
        rootfs.len()
    );

    // 3 + 4: boot a real Linux that mounts it over the emulator's virtio-blk.
    let kernel = gunzip(&cc14_dir().join("kernel/Image.gz"));
    let dtb = std::fs::read(cc14_dir().join("kernel/holospaces-virtio.dtb")).expect("dtb");

    let base = 0x8000_0000u64;
    let mut emu = Emulator::new(base, 512 * 1024 * 1024);
    emu.enable_sbi();
    emu.attach_disk(rootfs);
    emu.boot_kernel(&kernel, &dtb, base + 0x0700_0000)
        .expect("boot the kernel");
    let halt = emu.run(600_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();

    assert!(
        console.contains("Mounted root (ext4 filesystem)"),
        "the kernel mounts the assembled rootfs over virtio-blk; console:\n{console}"
    );
    let expected = std::fs::read_to_string(cc14_dir().join("expected.txt")).unwrap();
    let marker = expected.trim();
    assert!(
        console.contains(marker),
        "userspace runs from the assembled rootfs and prints the marker {marker:?} \
         (matching the QEMU differential oracle); console:\n{console}"
    );
    assert_eq!(
        halt,
        Halt::Exit(0),
        "the guest powers off cleanly via SBI SRST (it ran to completion)"
    );
}

/// Determinism (Law L1): assembling + booting twice yields the identical
/// console — the emulator + assembled rootfs are reproducible.
#[test]
#[ignore]
fn the_virtio_boot_is_reproducible() {
    let boot = || {
        let store = MemKappaStore::new();
        let img = ingest(&store).unwrap();
        let rootfs = assemble_rootfs(&store, &img);
        let kernel = gunzip(&cc14_dir().join("kernel/Image.gz"));
        let dtb = std::fs::read(cc14_dir().join("kernel/holospaces-virtio.dtb")).unwrap();
        let base = 0x8000_0000u64;
        let mut emu = Emulator::new(base, 512 * 1024 * 1024);
        emu.enable_sbi();
        emu.attach_disk(rootfs);
        emu.boot_kernel(&kernel, &dtb, base + 0x0700_0000).unwrap();
        emu.run(600_000_000);
        emu.console().to_vec()
    };
    assert_eq!(
        boot(),
        boot(),
        "the virtio boot is byte-for-byte reproducible"
    );
}
