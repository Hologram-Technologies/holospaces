//! `CC-51` (host witness) — the host-side 9p workspace API addresses the **full
//! nested tree** the guest reads/writes over `virtio-9p`, at `CC-15` parity with
//! the guest's `Twalk`/`Tcreate` (one content, Law L1).
//!
//! This is the substrate primitive the Source Control provider (`CC-51`) builds
//! on: a real repository is a *nested* `.git` object tree, not a flat set of root
//! files. The flat `workspace_file`/`workspace_write` (`CC-15`/`CC-17`) only
//! address the share root; the Git engine — running as native browser-peer exec
//! (the `CC-48` discipline) — drives `.git/objects/…`, `.git/refs/…`, `src/…`
//! through the nested-path API. This witness proves that tree is the **same
//! content** the booted OS sees over real 9p, **both directions**:
//!
//!   • host → guest: the host writes `.git/objects/ab/cdef` (creating the
//!     intermediate directories); the OS, mounting the share over `virtio-9p`,
//!     reads it back byte-identically (its busybox `cat` walks the nested tree —
//!     the guest's `Twalk` reaching host-`write_path`-created inodes);
//!   • guest → host: the OS creates `src/deep/from-guest.txt` over 9p; the host
//!     reads it back with `workspace_file_path` (resolving the nested path the
//!     guest's `Tmkdir`/`Tlcreate` built).
//!
//! Authority: the 9P2000.L protocol (the differential oracle is
//! `qemu-system-riscv64`'s own 9p server — same kernel + busybox). Heavy (a
//! real-OS boot), so `#[ignore]`d; the target/suite runs it with `--ignored`.

use std::io::Read;
use std::path::{Path, PathBuf};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};

fn cc18_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc18")
}
fn cc14_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14")
}

fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(cc18_dir().join("image/oci-layout")).unwrap();
    let index = std::fs::read(cc18_dir().join("image/index.json")).unwrap();
    let blob = |digest: &str| -> Option<Vec<u8>> {
        let hex = digest.strip_prefix("sha256:")?;
        std::fs::read(cc18_dir().join("image/blobs/sha256").join(hex)).ok()
    };
    ingest_image(store, &layout, &index, holospaces::Arch::Riscv64, blob)
}

fn gunzip(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out).unwrap();
    out
}

// A custom `/init` (busybox shell): mount the 9p share, read the host-written
// NESTED file, write a NESTED file back, print deterministic markers, then idle
// (so init never exits → no panic; the run budget bounds the test). It calls
// every applet explicitly through the busybox the CC-18 image ships at
// `/bin/busybox` (no `--install`, so no PATH/symlink dependency).
const NESTED_INIT: &[u8] = b"#!/bin/busybox sh\n\
/bin/busybox mkdir -p /proc /sys /dev /workspace\n\
/bin/busybox mount -t proc proc /proc\n\
/bin/busybox mount -t 9p -o trans=virtio,version=9p2000.L,msize=65536 hsworkspace /workspace 2>/dev/null\n\
C=$(/bin/busybox cat /workspace/.git/objects/ab/cdef 2>/dev/null)\n\
/bin/busybox echo \"NESTED-READ:$C\"\n\
/bin/busybox mkdir -p /workspace/src/deep\n\
/bin/busybox printf 'GUEST-NESTED-WROTE\\n' > /workspace/src/deep/from-guest.txt\n\
/bin/busybox echo 9P-NESTED-OK\n\
while /bin/busybox true; do /bin/busybox sleep 1; done\n";

/// The host's nested-path 9p API and the booted OS share one nested tree over
/// real `virtio-9p` (both directions). Heavy (a real-OS boot), so `#[ignore]`d.
#[test]
#[ignore]
fn the_host_and_os_share_a_nested_workspace_tree_over_virtio_9p() {
    // Assemble the busybox rootfs (CC-18 image) with our nested-ops init.
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the CC-18 busybox image");
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
    // A bootable, writable disk (64 MiB) so the OS has room to create its mount
    // points and the 9p mount succeeds (a content-sized image has no free space).
    let rootfs =
        assemble_ext4_bootable(&layers, NESTED_INIT, 64 * 1024 * 1024).expect("assemble rootfs");

    // Boot with the shared workspace (no flat seed); the kernel is loaded but not
    // yet stepped, so the host writes the NESTED content the init will read.
    let kernel = gunzip(&cc14_dir().join("kernel/Image.gz"));
    let mut emu = MachineSpec::devcontainer()
        .boot_workspace(&kernel, rootfs, &[])
        .expect("boot with the workspace share");

    // host → guest: write a deep object path, creating `.git/objects/ab/` on the
    // way (the host-side `Twalk`/`Tcreate` dual).
    emu.workspace_write_path(".git/objects/ab/cdef", b"NESTED-CONTENT-OK");
    assert_eq!(
        emu.workspace_file_path(".git/objects/ab/cdef"),
        Some(&b"NESTED-CONTENT-OK"[..]),
        "the host reads back its own nested write through the resolve-path API",
    );
    // The intermediate directories are real directory inodes (so the guest can
    // walk them), and the flat root view shows only `.git`, not a slash-name.
    assert_eq!(
        emu.workspace_stat_path(".git/objects/ab").map(|(d, _)| d),
        Some(true),
        "an intermediate path component is a directory inode (not a slash-named file)",
    );

    emu.run(600_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();

    assert!(
        console.contains("NESTED-READ:NESTED-CONTENT-OK"),
        "the OS read the host-written NESTED file over virtio-9p (host → guest, L1); console:\n{console}",
    );
    assert!(
        console.contains("9P-NESTED-OK"),
        "the OS completed its nested 9p operations; console:\n{console}",
    );

    // guest → host: the OS created a nested file the host reads back.
    assert_eq!(
        emu.workspace_file_path("src/deep/from-guest.txt"),
        Some(&b"GUEST-NESTED-WROTE\n"[..]),
        "the host reads back the NESTED file the OS wrote over 9p (guest → host, L1)",
    );
    // The host can also enumerate the guest-created nested directory.
    let listing = emu.workspace_list_path("src/deep").expect("list the nested dir");
    assert!(
        listing.iter().any(|(name, dir, _)| name == "from-guest.txt" && !*dir),
        "the host lists the guest-created nested directory; got {listing:?}",
    );
}
