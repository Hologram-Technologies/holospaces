//! `CC-14` (Layer Assembler clause) — a devcontainer's OCI image layers
//! assemble into a real, bootable `ext4` root filesystem held as κ-disk content.
//!
//! This witnesses the *Rootfs Assembly* sub-process of the conceptual model's
//! in-zoom OPD *SD5* and the *Layer Assembler* building block (arc42 ch.5): the
//! connector from `CC-10` (the verified, ingested OCI layers) to `CC-7` (the
//! κ-disk the emulator boots over `virtio-blk`). The whole flow — gunzip → untar
//! → OCI whiteout/opaque overlay → ext4 — is in-crate (Law L4: holospaces
//! produces the filesystem itself; it never shells out to `mke2fs`).
//!
//! The external validation authorities (V&V oracles, not runtime dependencies)
//! both run as the *oracle*; the filesystem is produced entirely by holospaces:
//!
//! - **e2fsprogs** `e2fsck` — the canonical ext2/3/4 implementation; it must
//!   find the produced image structurally clean (`rc == 0`, no fixes).
//! - **e2fsprogs** `debugfs` — reads the file content back out, independently of
//!   the writer, and it must equal the OCI layer's original bytes.

use std::path::{Path, PathBuf};
use std::process::Command;

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4, Layer};
use holospaces::disk::{BlockDevice, KappaDisk};
use holospaces::oci::{ingest_image, IngestedImage, OciError};

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

/// Resolve the ingested layer blobs from the store, in manifest order, paired
/// with their media types — the input to the Layer Assembler.
fn layer_blobs(store: &MemKappaStore, img: &IngestedImage) -> Vec<(String, Vec<u8>)> {
    img.layers()
        .iter()
        .zip(img.layer_media_types())
        .map(|(k, mt)| {
            let bytes = store
                .get(k)
                .unwrap()
                .expect("layer in store")
                .as_ref()
                .to_vec();
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

/// The real BuildKit OCI image's layers assemble into an `ext4` image that
/// e2fsprogs finds clean and whose files read back byte-for-byte — and the same
/// layers always yield the same image (Law L1).
#[test]
fn the_oci_layers_assemble_into_a_clean_mountable_ext4_rootfs() {
    if !have_tool("e2fsck") || !have_tool("debugfs") {
        eprintln!("SKIP: e2fsprogs (e2fsck/debugfs) not available");
        return;
    }

    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the real BuildKit OCI image");
    let blobs = layer_blobs(&store, &img);
    let layers: Vec<Layer> = blobs
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();

    // Assemble — gunzip + untar + overlay + ext4, entirely in-crate.
    let image = assemble_ext4(&layers).expect("assemble the rootfs ext4");
    assert!(
        image.len().is_multiple_of(4096),
        "ext4 image is a whole number of blocks"
    );

    // Reproducible: the same layers yield byte-identical bytes (Law L1, Q4).
    let again = assemble_ext4(&layers).expect("re-assemble");
    assert_eq!(image, again, "assembly is deterministic");

    // ── External oracle 1: e2fsck must find the structure clean ──
    let dir = std::env::temp_dir();
    let path = dir.join("cc14-rootfs.ext4");
    std::fs::write(&path, &image).unwrap();
    let fsck = Command::new("e2fsck")
        .args(["-fn"])
        .arg(&path)
        .output()
        .expect("run e2fsck");
    assert!(
        fsck.status.success(),
        "e2fsck must find the assembled ext4 clean (rc {:?}):\n{}\n{}",
        fsck.status.code(),
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr),
    );

    // ── External oracle 2: debugfs reads payload.txt back, equal to the layer ──
    // The OCI layer's payload.txt original bytes (decompressed independently).
    let expected = read_layer_file(&blobs, "payload.txt");
    let dump = dir.join("cc14-payload.out");
    let out = Command::new("debugfs")
        .arg("-R")
        .arg(format!("dump /payload.txt {}", dump.display()))
        .arg(&path)
        .output()
        .expect("run debugfs");
    assert!(
        out.status.success(),
        "debugfs dump failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = std::fs::read(&dump).expect("debugfs dumped payload.txt");
    assert_eq!(
        got, expected,
        "the file content read back by e2fsprogs equals the OCI layer's bytes"
    );

    // ── CC-7 tie-in: the ext4 becomes κ-disk content, reproducibly ──
    pollster::block_on(async {
        let disk = KappaDisk::from_image(&store, 512, &image)
            .await
            .expect("the assembled rootfs becomes a κ-disk (CC-7)");
        let mut back = vec![0u8; image.len()];
        let sectors = (image.len() / 512) as u32;
        disk.read(0, sectors, &mut back).await.unwrap();
        assert_eq!(
            back, image,
            "the rootfs is preserved byte-for-byte on the κ-disk"
        );

        // The image κ is a function of content alone — same bytes on another
        // peer's store yield the same disk identity (Law L1).
        let store2 = MemKappaStore::new();
        let disk2 = KappaDisk::from_image(&store2, 512, &image).await.unwrap();
        assert_eq!(
            disk.image_kappa(),
            disk2.image_kappa(),
            "the assembled rootfs κ-disk is reproducible across peers"
        );
    });

    std::fs::remove_file(&path).ok();
    std::fs::remove_file(&dump).ok();
}

/// Decompress the OCI layer that contains `name` and return that file's bytes —
/// an independent extraction (gzip + tar) to compare the assembler's result to.
fn read_layer_file(blobs: &[(String, Vec<u8>)], name: &str) -> Vec<u8> {
    use std::io::Read;
    for (mt, blob) in blobs {
        let tar = if mt.ends_with("gzip") || (blob.len() > 2 && blob[0] == 0x1f && blob[1] == 0x8b)
        {
            let mut d = flate2::read::GzDecoder::new(&blob[..]);
            let mut v = Vec::new();
            d.read_to_end(&mut v).unwrap();
            v
        } else {
            blob.clone()
        };
        // Minimal USTAR scan for the named file.
        let mut pos = 0;
        while pos + 512 <= tar.len() {
            let hdr = &tar[pos..pos + 512];
            if hdr.iter().all(|&b| b == 0) {
                break;
            }
            let fname = {
                let end = hdr[..100].iter().position(|&c| c == 0).unwrap_or(100);
                String::from_utf8_lossy(&hdr[..end]).into_owned()
            };
            let size = usize::from_str_radix(
                std::str::from_utf8(&hdr[124..135])
                    .unwrap()
                    .trim_matches(|c| c == ' ' || c == '\0'),
                8,
            )
            .unwrap_or(0);
            pos += 512;
            if fname.trim_start_matches("./") == name {
                return tar[pos..pos + size].to_vec();
            }
            pos += size.div_ceil(512) * 512;
        }
    }
    panic!("layer file {name} not found");
}
