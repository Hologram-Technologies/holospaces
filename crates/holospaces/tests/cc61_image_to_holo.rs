//! `CC-61` — a real docker (OCI) image becomes **one κ-addressable `.holo`** via
//! the **sparse-file streaming** path, byte-identical to the dense assembly and
//! structurally sound — the foundation of "import any image → run 100% serverless
//! in any browser".
//!
//! The deployed browser peer never holds a multi-GiB rootfs in the wasm heap: it
//! assembles the ext4 **block-by-block straight into a sparse OPFS file** (only the
//! non-zero blocks are written; the holes read back as zeros), then pages sectors
//! from that file on demand ("the KappaStore IS the memory, RAM is a cache", Laws
//! L3/L4). This witnesses that exact pattern natively — `std::fs::File` with
//! `seek` + `set_len` stands in for the OPFS file — on the **real BuildKit OCI
//! image** (`CC-10` fixture), and proves four properties that make the `.holo`
//! trustworthy:
//!
//!   1. **byte-identical** to the dense `assemble_ext4_bootable` (Law L1 — one
//!      canonical serialization, two consumers);
//!   2. **bounded peak memory** — the materialized (non-zero) bytes are ≪ the
//!      declared disk size, so a build-capable disk costs its *content*, not its
//!      size;
//!   3. **structurally sound** — `e2fsck -fn` finds the streamed image clean (the
//!      external e2fsprogs oracle, as `CC-14`);
//!   4. **one reproducible κ** — the assembled disk's
//!      [`image_kappa`](holospaces::disk::KappaDisk::image_kappa) is the same on
//!      any peer's store (the `.holo` handle: content addresses the whole disk).

use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_bootable, stream_ext4_image_bootable, Layer};
use holospaces::disk::KappaDisk;
use holospaces::oci::{ingest_image, IngestedImage, OciError};

/// The CC-10 fixture: a real, pinned BuildKit OCI image (the same one CC-14 uses).
fn image_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc10/image")
}

fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(image_dir().join("blobs/sha256").join(hex)).ok()
}

fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(image_dir().join("oci-layout")).expect("oci-layout");
    let index = std::fs::read(image_dir().join("index.json")).expect("index.json");
    ingest_image(
        store,
        &layout,
        &index,
        holospaces::Arch::Riscv64,
        blob_bytes,
    )
}

fn layer_blobs(store: &MemKappaStore, img: &IngestedImage) -> Vec<(String, Vec<u8>)> {
    img.layers()
        .iter()
        .zip(img.layer_media_types())
        .map(|(k, mt)| {
            let bytes = store.get(k).unwrap().expect("layer in store").as_ref().to_vec();
            (mt.to_string(), bytes)
        })
        .collect()
}

fn have_tool(name: &str) -> bool {
    Command::new(name)
        .arg("-V")
        .output()
        .map(|o| o.status.success() || !o.stderr.is_empty())
        .unwrap_or(false)
}

/// The freestanding `/init` injected so the assembled disk is a *bootable* rootfs
/// (its content is irrelevant to this assembly-fidelity witness).
const INIT: &[u8] = b"#!/bin/sh\nexec /bin/sh\n";
/// A build-capable declared disk — far larger than the small fixture's content, so
/// the sparse savings (property 2) are real and measurable.
const DISK: u64 = 64 * 1024 * 1024;
const SECTOR: u32 = 512;

#[test]
fn a_real_oci_image_streams_into_one_reproducible_holo() {
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the real BuildKit OCI image");
    let blobs = layer_blobs(&store, &img);
    let layers: Vec<Layer> = blobs
        .iter()
        .map(|(mt, b)| Layer { media_type: mt, blob: b })
        .collect();

    // ── Dense reference: the whole image materialized in RAM. ──
    let dense = assemble_ext4_bootable(&layers, INIT, DISK).expect("dense assembly");
    assert!(dense.len() as u64 == DISK || dense.len().is_multiple_of(4096));

    // ── The serverless path: assemble block-by-block into a SPARSE FILE, exactly
    // as the browser peer streams into an OPFS file. Only non-zero blocks are
    // written; `set_len` leaves the holes sparse (zero on read). Track the
    // materialized bytes to prove peak memory tracks content, not disk size. ──
    let img_path = std::env::temp_dir().join("cc61-image.holo.ext4");
    let mut materialized: u64 = 0;
    {
        let mut f = std::fs::File::create(&img_path).expect("create the sparse image file");
        let geom = stream_ext4_image_bootable(&layers, INIT, DISK, |bi, b| {
            materialized += b.len() as u64;
            f.seek(SeekFrom::Start(bi * 4096)).unwrap();
            f.write_all(b).unwrap();
        })
        .expect("sparse-stream the image into the file");
        f.set_len(geom.image_len()).unwrap(); // the trailing sparse region reads as zeros
        assert_eq!(geom.image_len(), dense.len() as u64, "same declared image size");
    }

    // Property 2 — bounded peak memory: the non-zero content is a small fraction of
    // the declared disk (the sparse assembler never materializes the holes).
    assert!(
        materialized < DISK / 4,
        "materialized non-zero bytes ({materialized}) must be ≪ the {DISK}-byte disk \
         — peak memory tracks content, not size"
    );

    // Property 1 — byte-identical to the dense assembly (Law L1): the sparse file,
    // read back in full, equals the dense image bit for bit.
    let from_file = std::fs::read(&img_path).expect("read the sparse image back");
    assert_eq!(
        from_file.len(),
        dense.len(),
        "the sparse file's declared length equals the dense image"
    );
    assert!(
        from_file == dense,
        "the sparse-streamed file is byte-identical to the dense assembly (Law L1)"
    );

    // Property 4 — one reproducible κ (the `.holo` handle): the assembled disk's
    // image_kappa is a function of content alone, identical on any peer's store.
    pollster::block_on(async {
        let s1 = MemKappaStore::new();
        let d1 = KappaDisk::from_image(&s1, SECTOR, &dense).await.expect("κ-disk on peer 1");
        let s2 = MemKappaStore::new();
        let d2 = KappaDisk::from_image(&s2, SECTOR, &from_file).await.expect("κ-disk on peer 2");
        assert_eq!(
            d1.image_kappa(),
            d2.image_kappa(),
            "the same image yields the same `.holo` κ on any peer (content-addressed)"
        );
    });

    // Property 3 — structurally sound: e2fsprogs finds the streamed `.holo` clean.
    if have_tool("e2fsck") {
        let fsck = Command::new("e2fsck").args(["-fn"]).arg(&img_path).output().expect("run e2fsck");
        assert!(
            fsck.status.success(),
            "e2fsck must find the streamed .holo image clean (rc {:?}):\n{}",
            fsck.status.code(),
            String::from_utf8_lossy(&fsck.stdout),
        );
    } else {
        eprintln!("SKIP e2fsck oracle: e2fsprogs not on PATH (byte-identity + κ still proven)");
    }

    std::fs::remove_file(&img_path).ok();
}
