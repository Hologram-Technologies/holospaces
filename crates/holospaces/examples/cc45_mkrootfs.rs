//! Export the `CC-45` amd64 devcontainer rootfs (the stock `linux-amd64`
//! busybox layer + the `CC-45` init, assembled into an `ext4` image by the
//! in-crate Layer Assembler) to a file — so `vv/suites/cc45-x64-devcontainer.sh`
//! can hand the *same* rootfs to `qemu-system-x86_64` as the differential oracle.
//! Usage: `cargo run --example cc45_mkrootfs -- <out.ext4>`.

use std::path::{Path, PathBuf};

use holospaces::assembly::{assemble_ext4_bootable, Layer};

fn cc45_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc45")
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .expect("usage: cc45_mkrootfs <out.ext4>");
    let init = std::fs::read(cc45_dir().join("init.sh")).expect("cc45 busybox init.sh");
    let layer = std::fs::read(cc45_dir().join("rootfs/layer.tar.gz")).expect("cc45 busybox layer");
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
        blob: &layer,
    }];
    let img = assemble_ext4_bootable(&layers, &init, 64 * 1024 * 1024).expect("assemble rootfs");
    std::fs::write(&out, &img).expect("write rootfs image");
    eprintln!("wrote {} ({} bytes)", out, img.len());
}
