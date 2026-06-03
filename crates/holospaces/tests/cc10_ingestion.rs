//! `CC-10` — a devcontainer's OS image + repository ingest as κ content (arc42
//! chapter 10, Conformance catalog; ADR-009).
//!
//! Witnesses the *ingestion and identity* clauses against real external
//! authorities — no mocks:
//!
//! * the **OCI image specification** (https://github.com/opencontainers/image-spec):
//!   a real OCI image-layout produced by **BuildKit** (`vv/artifacts/cc10/image`,
//!   provenance in `SOURCE.txt`) is walked — index → manifest → config + layers —
//!   and every blob is verified by re-derivation against its OCI `sha256` digest
//!   (the registry's content address *is* a κ-label; Law L5). A **forged** blob
//!   is refused.
//! * the **Dev Container specification** (https://containers.dev): the repository's
//!   `devcontainer.json` is validated (`CC-4`) and bound to the ingested image
//!   into a reproducible source identity — the same source yields the same κ on
//!   any peer (Law L1; QS1).
//!
//! The ingested image content is held as [κ-disk](holospaces::disk) blocks
//! (`CC-7`): the layer the emulator (`CC-9`) boots round-trips byte-for-byte.

use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{verify_kappa_axis, KappaStore};
use holospaces::disk::{BlockDevice, KappaDisk};
use holospaces::oci::{devcontainer_source_identity, ingest_image, IngestedImage, OciError};

const SECTOR_SIZE: u32 = 512;

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

/// The real OCI image ingests: every blob is verified by re-derivation against
/// its OCI digest (Law L5), and the manifest/config/layers are κ-addressed in the
/// store. (CC-10, the OCI authority.)
#[test]
fn a_real_oci_image_ingests_as_verified_kappa_content() {
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the real BuildKit OCI image");

    // The manifest digest is itself a κ-label on the sha256 axis.
    assert!(img.digest().starts_with("sha256:"));

    // Every ingested blob is in the store and re-derives to its OCI digest — the
    // registry's content address verified into the substrate (L5).
    for k in std::iter::once(img.manifest())
        .chain(std::iter::once(img.config()))
        .chain(img.layers())
    {
        let bytes = store.get(k).unwrap().expect("blob content in the store");
        // (Re-derivation against the *holospaces* blake3 κ — the store's own key.)
        assert!(
            hologram_substrate_core::verify_kappa(&bytes, k).unwrap(),
            "stored blob re-derives to its κ"
        );
    }
    assert_eq!(img.layers().len(), 1, "the image has one layer");

    // The OCI manifest blob, fetched back, still re-derives to the OCI digest on
    // the sha256 axis — the exact OCI guarantee.
    let manifest_bytes = store.get(img.manifest()).unwrap().unwrap();
    assert!(
        verify_kappa_axis(&manifest_bytes, img.digest().as_bytes()).unwrap(),
        "the manifest re-derives to its OCI sha256 digest"
    );
}

/// A forged image blob — bytes that no longer re-derive to their OCI digest — is
/// refused at ingestion (Law L5). (CC-10, tamper-evidence.)
#[test]
fn a_forged_image_blob_is_refused() {
    let store = MemKappaStore::new();
    let layout = std::fs::read(image_dir().join("oci-layout")).unwrap();
    let index = std::fs::read(image_dir().join("index.json")).unwrap();
    // Serve tampered bytes for every blob: the manifest no longer matches.
    let err = ingest_image(
        &store,
        &layout,
        &index,
        holospaces::Arch::Riscv64,
        |_digest| Some(b"tampered blob content".to_vec()),
    )
    .expect_err("a forged image must be refused");
    assert!(
        matches!(err, OciError::DigestMismatch(_)),
        "a blob that does not re-derive to its OCI digest is refused (L5), got {err:?}"
    );
}

/// The ingested image identity is reproducible, and the devcontainer source
/// (validated `devcontainer.json` + the image) yields the same holospace identity
/// on any peer. (CC-10, the Dev Container identity authority.)
#[test]
fn the_devcontainer_source_identity_is_reproducible() {
    let store_a = MemKappaStore::new();
    let store_b = MemKappaStore::new();
    let img_a = ingest(&store_a).unwrap();
    let img_b = ingest(&store_b).unwrap();
    assert_eq!(img_a.identity(), img_b.identity(), "same image ⇒ same κ");

    let config = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc10/devcontainer.json"),
    )
    .expect("read the real devcontainer.json");

    let id_a = devcontainer_source_identity(&config, &img_a).expect("valid devcontainer source");
    let id_b = devcontainer_source_identity(&config, &img_b).expect("valid devcontainer source");
    assert_eq!(
        id_a, id_b,
        "same (repo + image) source ⇒ same holospace κ (QS1)"
    );
}

/// The ingested image content lives as κ-disk blocks: the real layer blob the
/// emulator boots round-trips through the `KappaStore`-backed block device
/// byte-for-byte. (CC-10 ⇄ CC-7: the image is the disk.)
#[test]
fn the_ingested_layer_is_kappa_disk_content() {
    pollster::block_on(async {
        let store = MemKappaStore::new();
        let img = ingest(&store).unwrap();
        let layer = store.get(&img.layers()[0]).unwrap().expect("layer content");

        // Pad the layer to a sector boundary and write it onto a κ-disk.
        let mut image = layer.as_ref().to_vec();
        let pad =
            (SECTOR_SIZE as usize - image.len() % SECTOR_SIZE as usize) % SECTOR_SIZE as usize;
        image.resize(image.len() + pad, 0);
        let disk = KappaDisk::from_image(&store, SECTOR_SIZE, &image)
            .await
            .expect("the ingested layer becomes κ-disk content");

        let mut back = vec![0u8; image.len()];
        let sectors = (image.len() / SECTOR_SIZE as usize) as u32;
        disk.read(0, sectors, &mut back).await.unwrap();
        assert_eq!(back, image, "the OS-image layer is preserved on the κ-disk");
    });
}
