//! **CC-31 (resume terminal) — a resumed devcontainer's terminal is live, and
//! the substrate excludes the console *scrollback* from the machine snapshot.**
//!
//! The deployed browser peer persists a running devcontainer to a κ snapshot
//! (`Emulator::snapshot`) every couple of minutes and *resumes* from it on the
//! next visit (`Emulator::restore`) instead of cold-booting. By the time a
//! snapshot is taken in steady state the devcontainer is an **idle interactive
//! shell** (the CPU parked in WFI, waiting for console input) — *not* a machine
//! mid-boot still emitting output.
//!
//! This witness pins two facts the deployed resume depends on, which the
//! mid-boot-only `resume-test.mjs` did not cover (the gap that let a blank
//! resumed terminal reach `main`):
//!
//! 1. **The machine snapshot is κ-pure: it does not carry the console output
//!    buffer.** The scrollback is a projection of the *past* (it does not affect
//!    future computation), so it is deliberately not part of the machine's
//!    canonical state — a restored machine's console starts empty. The terminal
//!    *layer* (not the machine) is therefore responsible for restoring what the
//!    user sees on resume; a resumed idle shell that emits nothing yet must not
//!    read as a dead/blank terminal.
//! 2. **A resumed idle shell is live.** After restore, feeding a keystroke wakes
//!    the parked shell and it produces a fresh prompt / runs the command — the
//!    machine resumed, it was not re-booted, and it is interactive.
//!
//! External authority: the same CC-22 BusyBox devcontainer image the deploy
//! boots (`vv/artifacts/cc22`), driven through the real `Emulator`. Run by the
//! CC-11/CC-31 vv suite and `cargo test -p holospaces --test cc31_resume_terminal`.

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::Emulator;
use holospaces::machine::{MachineSpec, DEVCONTAINER_INIT};
use holospaces::oci::ingest_image;
use std::io::Read;
use std::path::Path;

fn cc22_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc22")
}

/// Boot the *deployed* devcontainer (CC-22 BusyBox image + the injected
/// persistent init) exactly as the browser peer's cold path does.
fn boot_deployed_devcontainer() -> Emulator {
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
    let layers: Vec<Layer> = owned
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();
    let rootfs = assemble_ext4_bootable(&layers, DEVCONTAINER_INIT, 64 * 1024 * 1024)
        .expect("assemble the bootable rootfs");
    let kernel_gz =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14/kernel/Image.gz");
    let mut kernel = Vec::new();
    flate2::read::GzDecoder::new(&std::fs::read(&kernel_gz).unwrap()[..])
        .read_to_end(&mut kernel)
        .unwrap();
    MachineSpec::devcontainer()
        .boot_workspace(&kernel, rootfs, &[("WELCOME.md", b"hi")])
        .expect("boot the deployed devcontainer")
}

#[test]
#[ignore = "boots the deployed devcontainer + suspends/resumes it (~release) — run by the CC-31 vv suite"]
fn a_resumed_idle_devcontainer_is_live_and_its_scrollback_is_a_terminal_concern() {
    // Boot all the way to the *idle interactive shell* — the steady state in
    // which the deploy actually takes its periodic snapshot.
    let mut emu = boot_deployed_devcontainer();
    emu.run(1_500_000_000);
    assert!(
        emu.console()
            .windows(b"holospace devcontainer ready".len())
            .any(|w| w == b"holospace devcontainer ready"),
        "the devcontainer booted to its idle interactive shell (the snapshot point)"
    );
    let scrollback_len = emu.console().len();
    assert!(
        scrollback_len > 0,
        "the running shell has produced console scrollback before the snapshot"
    );

    // Snapshot the idle machine and restore it — the browser peer's suspend/resume.
    let snapshot = emu.snapshot();
    let base = MachineSpec::devcontainer().base;
    let mut resumed = Emulator::restore(base, &snapshot).expect("restore the κ snapshot");

    // (1) The machine snapshot is κ-pure: the console *output* buffer is not part
    //     of it, so the restored machine's console starts empty. This is *why* a
    //     naive resume shows a blank terminal — the terminal layer, not the
    //     machine, must restore the visible scrollback.
    assert!(
        resumed.console().is_empty(),
        "the restored machine's console scrollback is empty — the output buffer is \
         excluded from the machine snapshot (a past projection, not machine state); \
         the terminal layer is responsible for restoring what the user sees"
    );

    // (2) The resumed idle shell is *live*: a keystroke wakes the parked CPU and
    //     the shell runs the command — it resumed, it was not re-booted.
    resumed.feed_console(b"echo RESUMED-OK\n");
    resumed.run(400_000_000);
    let produced = String::from_utf8_lossy(resumed.console());
    assert!(
        produced.contains("RESUMED-OK"),
        "the resumed shell is live and interactive — feeding a command produces \
         output (it resumed mid-session, it was not re-booted); saw:\n{produced:?}"
    );
}
