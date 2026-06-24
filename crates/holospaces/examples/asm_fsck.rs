//! Stream-assemble the cc45 amd64 rootfs onto a declared-size disk (arg2 bytes,
//! default 8 GiB) into a sparse file (arg1), for e2fsck validation of the geometry.
use holospaces::assembly::{stream_ext4_image_bootable, Layer};
use std::io::{Seek, SeekFrom, Write};
fn main() {
    let art = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc45");
    let layer = std::fs::read(art.join("rootfs/layer.tar.gz")).unwrap();
    let init = std::fs::read(art.join("init.sh")).unwrap();
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar+gzip",
        blob: &layer,
    }];
    let out = std::env::args().nth(1).expect("out path");
    let disk: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8 * 1024 * 1024 * 1024);
    let mut f = std::fs::File::create(&out).unwrap();
    f.set_len(disk).unwrap();
    let geom = stream_ext4_image_bootable(&layers, &init, disk, |bi, bytes| {
        f.seek(SeekFrom::Start(bi * 4096)).unwrap();
        f.write_all(bytes).unwrap();
    })
    .unwrap();
    f.set_len(geom.image_len() as u64).unwrap();
    eprintln!(
        "image_len={} total_blocks={}",
        geom.image_len(),
        geom.image_len() / 4096
    );
}
