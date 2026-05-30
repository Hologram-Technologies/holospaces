//! `CC-7` — the κ-disk preserves a real filesystem (arc42 chapter 10,
//! Conformance catalog; ADR-009).
//!
//! Witnesses that [`holospaces::disk::KappaDisk`] — a `KappaStore`-backed
//! `BlockDevice` — preserves a **real on-disk filesystem** byte-for-byte. The
//! external authority is a real ext4 image produced by **e2fsprogs** (the
//! canonical Linux ext2/3/4 implementation — `vv/artifacts/cc7/rootfs.ext4`,
//! provenance in `vv/artifacts/cc7/SOURCE.txt`). Two independent checks:
//!
//! * **byte-exact round trip** — the whole image, written through the κ-disk
//!   sector by sector and read back, is identical to the original (the disk is
//!   the `KappaStore` viewed through the block-device seam — no second medium);
//! * **the filesystem authority** — `debugfs` (e2fsprogs' own ext reader, an
//!   *independent implementation* from the κ-disk) reads the real files back out
//!   of the round-tripped image, proving a real ext4 filesystem survived intact.
//!
//! Plus the content-addressed properties the laws require: identical sectors are
//! stored once (dedup, L2/L3) and the image κ is reproducible (L1).

use std::io::Write;
use std::process::Command;

use hologram_store_mem::MemKappaStore;
use holospaces::disk::{BlockDevice, KappaDisk};

const SECTOR_SIZE: u32 = 512;

fn artifact_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vv/artifacts/cc7")
        .join(name)
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The committed ext4 artifact matches its recorded digest — the witness runs
/// against the exact authoritative bytes (provenance integrity).
#[test]
fn the_ext4_artifact_matches_its_recorded_digest() {
    let image = std::fs::read(artifact_path("rootfs.ext4")).expect("read ext4 artifact");
    let recorded = std::fs::read_to_string(artifact_path("rootfs.ext4.sha256")).expect("read sha");
    let recorded = recorded.split_whitespace().next().expect("digest token");
    assert_eq!(sha256_hex(&image), recorded, "artifact integrity");
    assert_eq!(
        image.len() % SECTOR_SIZE as usize,
        0,
        "image is sector-aligned"
    );
}

/// A real ext4 filesystem image round-trips through the κ-disk byte-for-byte,
/// and a real ext4 reader (`debugfs`) reads the files back out of the
/// round-tripped image. (CC-7, the filesystem authority.)
#[test]
fn a_real_ext4_filesystem_round_trips_through_the_kappa_disk() {
    pollster::block_on(async {
        let image = std::fs::read(artifact_path("rootfs.ext4")).expect("read ext4 artifact");
        let store = MemKappaStore::new();

        // Write the real filesystem image into the κ-disk (it becomes κ-addressed
        // content), then read the whole device back.
        let disk = KappaDisk::from_image(&store, SECTOR_SIZE, &image)
            .await
            .expect("ingest the ext4 image as κ content");
        let mut back = vec![0u8; image.len()];
        let sectors = (image.len() / SECTOR_SIZE as usize) as u32;
        disk.read(0, sectors, &mut back).await.expect("read back");
        assert_eq!(back, image, "the ext4 image is preserved byte-for-byte");

        // Content-addressed dedup: a fresh 1 MiB ext4 image is mostly zero/
        // repeated blocks, so the κ-disk stores far fewer distinct sectors than
        // it has (Laws L2/L3).
        assert!(
            disk.distinct_sectors() < sectors as usize,
            "identical sectors are stored once ({} distinct of {} sectors)",
            disk.distinct_sectors(),
            sectors
        );

        // The image κ is reproducible: re-ingesting the same bytes on another
        // store yields the same identity (Law L1).
        let store2 = MemKappaStore::new();
        let disk2 = KappaDisk::from_image(&store2, SECTOR_SIZE, &image)
            .await
            .unwrap();
        assert_eq!(
            disk.image_kappa(),
            disk2.image_kappa(),
            "same filesystem ⇒ same disk κ"
        );

        // The authority differential: debugfs (e2fsprogs' independent ext reader)
        // reads the real files out of the round-tripped image. This proves the
        // κ-disk preserved a real ext4 filesystem, not merely arbitrary bytes.
        if let Some(debugfs) = debugfs_bin() {
            let mut tmp = std::env::temp_dir();
            tmp.push(format!("holospaces-cc7-{}.ext4", std::process::id()));
            std::fs::File::create(&tmp)
                .and_then(|mut f| f.write_all(&back))
                .expect("stage the round-tripped image");

            for (path, expected) in [
                (
                    "/hello.txt",
                    "hello from a real ext4 filesystem on a kappa-disk\n",
                ),
                ("/fox.txt", "the quick brown fox jumps over the lazy dog\n"),
                (
                    "/dir/nested.txt",
                    "nested content preserved byte-for-byte\n",
                ),
            ] {
                let out = Command::new(&debugfs)
                    .arg("-R")
                    .arg(format!("cat {path}"))
                    .arg(&tmp)
                    .output()
                    .expect("run debugfs");
                let got = String::from_utf8_lossy(&out.stdout);
                assert!(
                    got.contains(expected),
                    "debugfs reads {path} out of the round-tripped ext4 image \
                     (the filesystem authority): expected {expected:?}, got {got:?}"
                );
            }
            let _ = std::fs::remove_file(&tmp);
        } else {
            eprintln!(
                "cc7: debugfs (e2fsprogs) not on PATH — the byte-exact round trip \
                 and reproducible κ are witnessed; the ext4-reader differential is skipped"
            );
        }
    });
}

/// Resolve `debugfs` if e2fsprogs is installed (it is the ext authority; absent
/// only in a minimal environment, where the suite skips this differential).
fn debugfs_bin() -> Option<String> {
    for cand in ["debugfs", "/sbin/debugfs", "/usr/sbin/debugfs"] {
        if Command::new(cand).arg("-V").output().is_ok() {
            return Some(cand.to_string());
        }
    }
    None
}
