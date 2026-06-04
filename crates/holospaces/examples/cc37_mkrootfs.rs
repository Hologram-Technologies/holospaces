//! Export the `CC-37` arm64 devcontainer rootfs (the stock `linux-arm64`
//! busybox layer + the `CC-37` init, assembled into an `ext4` image by the
//! in-crate Layer Assembler) to a file — so `vv/suites/cc37-aarch64-devcontainer.sh`
//! can hand the *same* rootfs to `qemu-system-aarch64` as the differential
//! oracle. Usage: `cargo run --example cc37_mkrootfs -- <out.ext4>`.

use std::path::{Path, PathBuf};

use holospaces::assembly::{assemble_ext4_bootable, Layer};

fn cc37_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc37")
}

fn main() {
    let out = std::env::args()
        .nth(1)
        .expect("usage: cc37_mkrootfs <out.ext4>");
    let init = std::fs::read(cc37_dir().join("init.sh")).expect("cc37 busybox init.sh");
    let layer = std::fs::read(cc37_dir().join("rootfs/layer.tar.gz")).expect("cc37 busybox layer");
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
        blob: &layer,
    }];
    let img = assemble_ext4_bootable(&layers, &init, 64 * 1024 * 1024).expect("assemble rootfs");
    std::fs::write(&out, &img).expect("write rootfs image");
    eprintln!("wrote {} ({} bytes)", out, img.len());
}
