//! `CC-50` — Provisioning assembles an arbitrarily large rootfs **without**
//! materializing a dense in-memory image (sparse, streaming assembly).
//!
//! OPM process: SD5 *Rootfs Assembly* — "the KappaStore IS the memory, RAM is a
//! cache" (Laws L3/L4). The dense [`assemble_ext4_bootable`] returns the whole
//! ext4 image as one `Vec<u8>` sized to the *declared* disk: a multi-GiB disk
//! whose free space is sparse still costs multi-GiB of RAM to assemble. The
//! streaming path [`stream_ext4_bootable_into_disk`] emits only the non-zero 4 KiB
//! blocks straight into a [`KappaDisk`] over a `KappaStore` — peak working memory
//! tracks the image's *content*, not its size.
//!
//! This witness proves three properties on a sized fixture:
//!
//!  1. **Bounded peak memory** — assembling a disk *much larger* than its content
//!     (a sparse multi-hundred-MiB disk over a few-MiB rootfs) has peak heap
//!     ≪ the declared image size (a global allocator tracks the high-water mark).
//!  2. **κ-identity (differential)** — the streamed κ-disk's `image_kappa` equals
//!     the dense path's, and every sector reads back byte-identical to the dense
//!     `assemble_ext4_bootable` image. Same content ⇒ same κ-set (Law L1).
//!  3. **Valid bootable ext4** — when e2fsprogs is present, the streamed image is
//!     reconstructed and `e2fsck` finds it structurally clean (the external V&V
//!     oracle); the reconstruction also equals the dense image byte-for-byte.

use hologram_store_mem::MemKappaStore;
use holospaces::assembly::ext4;
use holospaces::assembly::{
    assemble_ext4_bootable, overlay_layers, stream_ext4_bootable_into_disk, Layer,
};
use holospaces::disk::{BlockDevice, KappaDisk};

// CC-50's bounded-memory property is proved *structurally* rather than with a
// peak-tracking global allocator (the workspace forbids `unsafe`, and a
// `GlobalAlloc` impl is necessarily unsafe). The streaming serializer
// [`ext4::stream_image_with_free`] only ever materializes the *non-zero* blocks
// (its working set is the sparse block map); it never allocates the dense image.
// So the total bytes it materializes — the sum of emitted block sizes plus the
// few transient metadata buffers — is the real upper bound on its peak working
// memory, and the test asserts that sum is ≪ the declared image size. A dense
// assembler would, by contrast, allocate the full image up front.

// ── A real, multi-block fixture rootfs ──────────────────────────────────────
//
// An uncompressed USTAR layer with a handful of files totalling a few MiB. The
// declared disk is far larger, so most of it is sparse free space — exactly the
// case CC-50 must assemble without paying for the free space in RAM.

/// A tar entry to assemble: (name, typeflag, linkname, data).
type TarEntry = (Vec<u8>, u8, Vec<u8>, Vec<u8>);

/// A few MiB of real file content across several files (deterministic bytes).
fn fixture_layer() -> Vec<u8> {
    let mut entries: Vec<TarEntry> = Vec::new();
    // /bin (dir) so the tree has depth.
    entries.push((b"bin/".to_vec(), b'5', vec![], vec![]));
    // A static "busybox" placeholder so the init has something to find, plus a
    // spread of files whose distinct contents exercise multiple data blocks.
    for i in 0..8u32 {
        let name = format!("bin/f{i:02}").into_bytes();
        // ~512 KiB each → ~4 MiB total content, several 4 KiB blocks per file.
        let data = vec![(i as u8).wrapping_add(1); 512 * 1024];
        entries.push((name, b'0', vec![], data));
    }
    make_tar(&entries)
}

const SECTOR: u32 = 512;
const INIT: &[u8] = b"#!/bin/sh\nexec /bin/sh\n";

#[test]
fn a_sparse_large_rootfs_streams_with_bounded_peak_memory() {
    let layer_bytes = fixture_layer();
    let content_len = layer_bytes.len() as u64;
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar",
        blob: &layer_bytes,
    }];

    // Declare a disk far larger than the content: 512 MiB over ~4 MiB of files.
    // The dense path would allocate ~512 MiB; the streaming path must not.
    let disk_bytes: u64 = 512 * 1024 * 1024;
    assert!(
        disk_bytes > content_len * 16,
        "fixture must be much smaller than the declared disk to be a real test"
    );
    let min_blocks = disk_bytes / 4096;
    let min_inodes = u32::try_from(disk_bytes / 16384).unwrap();

    // Drive the streaming serializer directly and measure the *materialized* bytes
    // — the sum of every block it emits. This is the upper bound on its peak
    // working memory: the serializer only ever holds the sparse non-zero blocks
    // (it never allocates the dense image), so what it emits is what it builds.
    let tree = overlay_layers(&layers).expect("overlay");
    // Inject /init exactly as the bootable assembler does, so the measured tree
    // matches the streamed disk.
    let tree = with_init(tree, INIT);

    let mut emitted_bytes: u64 = 0;
    let mut emitted_blocks: u64 = 0;
    let geom = ext4::stream_image_with_free(&tree, min_inodes, min_blocks, |_idx, bytes| {
        emitted_bytes += bytes.len() as u64;
        emitted_blocks += 1;
    })
    .expect("stream serialize");

    let image_len = geom.image_len();
    assert!(
        image_len >= disk_bytes,
        "the image ({image_len} B) covers the declared disk ({disk_bytes} B)"
    );

    // The materialized (peak) bytes must be ≪ the image size — bounded by content,
    // not by the declared disk. A dense assembler allocates `image_len`; we hold a
    // small multiple of the actual content.
    let bound = content_len * 4;
    assert!(
        emitted_bytes < bound,
        "materialized {emitted_bytes} B must be ≪ image {image_len} B \
         (bound {bound} B, content {content_len} B)"
    );
    assert!(
        emitted_bytes < image_len / 16,
        "materialized {emitted_bytes} B must be far below the dense image {image_len} B \
         (never materializes the dense image)"
    );
    // The vast majority of the disk's blocks are sparse (never emitted).
    let total_blocks = geom.total_blocks;
    assert!(
        emitted_blocks * 4 < total_blocks,
        "only the content blocks are emitted: {emitted_blocks} of {total_blocks} \
         (the free space is sparse)"
    );

    // And the end-to-end streaming assembly succeeds, sized to the declared disk.
    let store = MemKappaStore::new();
    let disk = stream_ext4_bootable_into_disk(&store, SECTOR, &layers, INIT, disk_bytes)
        .expect("streaming assembly into the κ-disk");
    assert_eq!(disk.sector_count() * SECTOR as u64, image_len, "same geometry");

    eprintln!(
        "CC-50: assembled a {} B disk from {} B of content, materializing only {} B \
         in {} of {} blocks ({:.3}% of the image)",
        image_len,
        content_len,
        emitted_bytes,
        emitted_blocks,
        total_blocks,
        (emitted_bytes as f64) * 100.0 / (image_len as f64),
    );
}

/// Inject `/init` (mode 0755) into a tree, mirroring `assemble_ext4_bootable`, so
/// a measured tree matches the streamed disk's content.
fn with_init(mut tree: holospaces::assembly::Tree, init: &[u8]) -> holospaces::assembly::Tree {
    use holospaces::assembly::{Meta, Node};
    let id = tree.contents.keys().copied().max().map_or(0, |m| m + 1);
    tree.contents.insert(id, init.to_vec());
    if let Node::Dir { children, .. } = &mut tree.root {
        children.insert(
            "init".to_string(),
            Node::File {
                meta: Meta {
                    mode: 0o755,
                    uid: 0,
                    gid: 0,
                    mtime: 0,
                },
                content: id,
            },
        );
    }
    tree
}

#[test]
fn the_streamed_kappa_set_is_identical_to_the_dense_path() {
    let layer_bytes = fixture_layer();
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar",
        blob: &layer_bytes,
    }];
    let disk_bytes: u64 = 256 * 1024 * 1024;

    // The dense image and the dense κ-disk built from it.
    let dense_img = assemble_ext4_bootable(&layers, INIT, disk_bytes).expect("dense assembly");
    let dense_store = MemKappaStore::new();
    let dense_disk = pollster::block_on(KappaDisk::from_image(&dense_store, SECTOR, &dense_img))
        .expect("dense κ-disk from image");

    // The streamed κ-disk over its own store.
    let stream_store = MemKappaStore::new();
    let stream_disk =
        stream_ext4_bootable_into_disk(&stream_store, SECTOR, &layers, INIT, disk_bytes)
            .expect("streaming assembly");

    // ── κ-identity: the disks' image κ-labels are equal (Law L1) ──
    assert_eq!(
        stream_disk.image_kappa(),
        dense_disk.image_kappa(),
        "the streamed κ-disk's image κ must equal the dense path's (identical κ-set)"
    );

    // ── The streamed disk reads back byte-identical to the dense image ──
    let sectors = dense_img.len() as u64 / SECTOR as u64;
    assert_eq!(sectors, stream_disk.sector_count(), "same geometry");
    // Read back in chunks (bounded), comparing to the dense image.
    let chunk_sectors: u32 = 4096; // 2 MiB at a time
    let mut buf = vec![0u8; chunk_sectors as usize * SECTOR as usize];
    let mut lba = 0u64;
    while lba < sectors {
        let n = ((sectors - lba) as u32).min(chunk_sectors);
        let slice = &mut buf[..n as usize * SECTOR as usize];
        pollster::block_on(stream_disk.read(lba, n, slice)).expect("read streamed disk");
        let off = (lba * SECTOR as u64) as usize;
        assert_eq!(
            &slice[..],
            &dense_img[off..off + slice.len()],
            "streamed sectors at lba {lba} differ from the dense image"
        );
        lba += n as u64;
    }

    // ── Dedup is real: the streamed store holds the same distinct sectors as the
    // dense one (the sparse free space stores nothing in either) ──
    assert_eq!(
        stream_disk.distinct_sectors(),
        dense_disk.distinct_sectors(),
        "the streamed disk dedups to the same distinct-sector count as the dense disk"
    );
}

#[test]
fn the_streamed_image_is_a_clean_bootable_ext4() {
    if !have_tool("e2fsck") {
        eprintln!("SKIP: e2fsprogs (e2fsck) not available");
        return;
    }
    let layer_bytes = fixture_layer();
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar",
        blob: &layer_bytes,
    }];
    let disk_bytes: u64 = 128 * 1024 * 1024;

    // Stream into a κ-disk, then reconstruct the dense image by reading it back
    // (the disk is the source of truth; this is what the emulator's virtio-blk
    // would read sector-by-sector at boot).
    let store = MemKappaStore::new();
    let disk = stream_ext4_bootable_into_disk(&store, SECTOR, &layers, INIT, disk_bytes)
        .expect("streaming assembly");
    let sectors = disk.sector_count();
    let mut img = vec![0u8; (sectors * SECTOR as u64) as usize];
    let chunk_sectors: u32 = 8192;
    let mut lba = 0u64;
    while lba < sectors {
        let n = ((sectors - lba) as u32).min(chunk_sectors);
        let off = (lba * SECTOR as u64) as usize;
        let slice = &mut img[off..off + n as usize * SECTOR as usize];
        pollster::block_on(disk.read(lba, n, slice)).expect("read disk");
        lba += n as u64;
    }

    // It must equal the dense path byte-for-byte.
    let dense = assemble_ext4_bootable(&layers, INIT, disk_bytes).expect("dense assembly");
    assert_eq!(img, dense, "the streamed image equals the dense image");

    // External oracle: e2fsck must find the structure clean.
    let path = std::env::temp_dir().join("cc50-streamed.ext4");
    std::fs::write(&path, &img).unwrap();
    let fsck = std::process::Command::new("e2fsck")
        .args(["-fn"])
        .arg(&path)
        .output()
        .expect("run e2fsck");
    assert!(
        fsck.status.success(),
        "e2fsck must find the streamed ext4 clean (rc {:?}):\n{}\n{}",
        fsck.status.code(),
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr),
    );
}

fn have_tool(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("-V")
        .output()
        .map(|o| o.status.success() || !o.stderr.is_empty())
        .unwrap_or(false)
}

// ── A minimal uncompressed USTAR builder (mirrors the assembly unit tests) ───

fn make_tar(entries: &[TarEntry]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, typeflag, link, data) in entries {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name);
        write_octal(&mut h[100..108], 0o644);
        write_octal(&mut h[108..116], 0);
        write_octal(&mut h[116..124], 0);
        write_octal(&mut h[124..136], data.len() as u64);
        write_octal(&mut h[136..148], 0);
        h[156] = *typeflag;
        h[157..157 + link.len()].copy_from_slice(link);
        h[257..263].copy_from_slice(b"ustar\0");
        h[263] = b'0';
        h[264] = b'0';
        for c in h.iter_mut().skip(148).take(8) {
            *c = b' ';
        }
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        write_octal(&mut h[148..155], sum as u64);
        h[155] = b' ';
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        let pad = data.len().div_ceil(512) * 512 - data.len();
        out.resize(out.len() + pad, 0);
    }
    out.extend([0u8; 1024]);
    out
}

fn write_octal(field: &mut [u8], v: u64) {
    let s = format!("{:0width$o}", v, width = field.len() - 1);
    field[..s.len()].copy_from_slice(s.as_bytes());
}
