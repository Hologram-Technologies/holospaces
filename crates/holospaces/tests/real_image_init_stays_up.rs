//! `REAL_IMAGE_INIT` keeps a real-image devcontainer **up and interactive** —
//! the init runs the image's login shell with a controlling terminal
//! (`setsid -c`) in a **respawn loop**, so the guest does NOT halt after boot.
//!
//! The regression this guards: the init used to `exec` the login shell as PID 1
//! with no controlling terminal. Without a controlling tty a login shell reads
//! EOF and exits at once; because it was PID 1, its exit panicked the kernel
//! ("Attempted to kill init") and **halted the guest** — the user booted the
//! devcontainer only to watch it stop the instant they tried to use it (every
//! real-image arch: amd64, aarch64, riscv64). The fix runs the shell in a loop,
//! never `exec`ed, so PID 1 cannot die and the devcontainer stays usable.
//!
//! Witnessed natively over the real `Emulator`: a real-image rootfs boots to an
//! interactive shell, a typed command runs, and — the regression — after the
//! shell **exits** the guest stays up and a **fresh shell answers**.

use flate2::read::GzDecoder;
use hologram_store_mem::MemKappaStore;
use holospaces::assembly::{assemble_ext4_with_files, Layer};
use holospaces::emulator::Emulator;
use holospaces::machine::{MachineSpec, REAL_IMAGE_INIT};
use holospaces::oci::ingest_image;
use holospaces::substrate::KappaStore;
use std::io::Read;
use std::path::Path;

fn cc22_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc22")
}

/// Pull one regular file's bytes out of a gzipped OCI layer tar — a minimal
/// USTAR walk (enough for the BusyBox layer; no `tar` crate needed).
fn extract_from_layer(layer_gz: &[u8], want: &str) -> Option<Vec<u8>> {
    let mut tar = Vec::new();
    GzDecoder::new(layer_gz).read_to_end(&mut tar).ok()?;
    let mut i = 0;
    while i + 512 <= tar.len() {
        let hdr = &tar[i..i + 512];
        if hdr.iter().all(|&b| b == 0) {
            break;
        }
        let name_len = hdr[..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = String::from_utf8_lossy(&hdr[..name_len]);
        let norm = name.trim_start_matches("./").trim_end_matches('/');
        let size_oct = std::str::from_utf8(&hdr[124..136])
            .ok()?
            .trim_matches(|c: char| c == '\0' || c == ' ');
        let size = usize::from_str_radix(size_oct, 8).unwrap_or(0);
        let typeflag = hdr[156];
        i += 512;
        if (typeflag == b'0' || typeflag == 0) && norm == want {
            return tar.get(i..i + size).map(<[u8]>::to_vec);
        }
        i += size.div_ceil(512) * 512;
    }
    None
}

/// Boot a *real-image* devcontainer (`REAL_IMAGE_INIT`) the way the deployed
/// peer boots a pulled image. The CC-22 BusyBox image carries only
/// `/bin/busybox`; a real image (debian, alpine, …) carries `/bin/sh` + the
/// usual coreutils, so we materialize those (BusyBox provides each applet by
/// `argv[0]`) — `REAL_IMAGE_INIT` then runs unchanged.
fn boot_real_image_devcontainer() -> Emulator {
    let store = MemKappaStore::new();
    let layout = std::fs::read(cc22_dir().join("image/oci-layout")).unwrap();
    let index = std::fs::read(cc22_dir().join("image/index.json")).unwrap();
    let img = ingest_image(
        &store,
        &layout,
        &index,
        holospaces::Arch::Riscv64,
        |digest| {
            let hex = digest.strip_prefix("sha256:")?;
            std::fs::read(cc22_dir().join("image/blobs/sha256").join(hex)).ok()
        },
    )
    .expect("ingest the CC-22 BusyBox image");
    let owned: Vec<(String, Vec<u8>)> = img
        .layers()
        .iter()
        .zip(img.layer_media_types())
        .map(|(k, mt)| (mt.clone(), store.get(k).unwrap().unwrap().as_ref().to_vec()))
        .collect();
    let busybox = owned
        .iter()
        .find_map(|(_, b)| extract_from_layer(b, "bin/busybox"))
        .expect("the BusyBox binary in the image layer");
    let layers: Vec<Layer> = owned
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();
    let rootfs = assemble_ext4_with_files(
        &layers,
        &[
            ("init", 0o755, REAL_IMAGE_INIT),
            ("bin/sh", 0o755, &busybox),
            ("bin/setsid", 0o755, &busybox),
            ("bin/sleep", 0o755, &busybox),
            ("bin/mkdir", 0o755, &busybox),
            ("bin/mount", 0o755, &busybox),
        ],
    )
    .expect("assemble the real-image rootfs");
    let kernel_gz =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14/kernel/Image.gz");
    let mut kernel = Vec::new();
    GzDecoder::new(&std::fs::read(&kernel_gz).unwrap()[..])
        .read_to_end(&mut kernel)
        .unwrap();
    MachineSpec::devcontainer()
        .boot_workspace(&kernel, rootfs, &[("WELCOME.md", b"hi")])
        .expect("boot the real-image devcontainer")
}

fn contains(emu: &Emulator, needle: &str) -> bool {
    emu.console()
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Run in budget chunks until `needle` appears on the console (or the cap).
fn run_until(emu: &mut Emulator, needle: &str, chunks: u32) -> bool {
    for _ in 0..chunks {
        if contains(emu, needle) {
            return true;
        }
        emu.run(200_000_000);
    }
    contains(emu, needle)
}

#[test]
#[ignore = "boots a real-image devcontainer over the Emulator (~release) — run by the CC-11 vv suite"]
fn a_real_image_devcontainer_stays_interactive_and_survives_the_shell_exiting() {
    let mut emu = boot_real_image_devcontainer();
    assert!(
        run_until(&mut emu, "holospace devcontainer ready", 12),
        "the real-image devcontainer booted to its interactive shell"
    );

    // 1) It is interactive: a typed command runs and its output appears.
    emu.feed_console(b"echo CC53-ALIVE\n");
    assert!(
        run_until(&mut emu, "CC53-ALIVE", 6),
        "the booted shell ran a typed command (interactive)"
    );

    // 2) THE REGRESSION: exit the shell. The old init `exec`ed the shell as PID
    //    1, so its exit panicked the kernel and HALTED the guest. The respawn
    //    loop keeps PID 1 alive — a fresh shell must answer the next command.
    emu.feed_console(b"exit\n");
    emu.run(400_000_000); // the shell exits; the init loop respawns it
    emu.feed_console(b"echo CC53-RESPAWN\n");
    assert!(
        run_until(&mut emu, "CC53-RESPAWN", 8),
        "after the shell exited the devcontainer stayed up and a fresh shell answered \
         (PID 1 did not die and halt the guest)"
    );
}
