//! `CC-11` (raw interactive terminal) — the deployed devcontainer's console is a
//! *real* terminal: raw keystrokes are echoed and line-edited by the guest tty,
//! and Ctrl-C raises SIGINT in the foreground process.
//!
//! The integrated terminal feeds the OS console **raw bytes** (each keystroke,
//! control bytes, escape sequences) and renders the console's raw output — the OS
//! owns the line discipline, exactly as a Codespace's remote does. This witnesses
//! that contract on the **deployed** devcontainer (the `CC-22` BusyBox rootfs +
//! the persistent `DEVCONTAINER_INIT`, booted as the browser workbench boots it).
//!
//! Authority: the emulator is the `qemu-system-riscv64`-differential-validated
//! machine (`CC-9`/`CC-14` boot byte-identical; `CC-11` line input byte-identical
//! to the qemu oracle). The raw-input path reads the *same* console the kernel's
//! tty drives, so its echo / line-editing / signal behaviour is qemu-faithful by
//! construction — here exercised end to end:
//!   * **echo + line editing** — typing `echo abZ`, a backspace, then `c` runs
//!     `echo abc` (the backspace edited the line in the guest, not in JS), and the
//!     keystrokes were echoed by the guest;
//!   * **Ctrl-C → SIGINT** — Ctrl-C interrupts a command that would otherwise run
//!     far past the budget (`sleep 999999`), returning the shell to a prompt so a
//!     following command runs — proving the signal reached the foreground process,
//!     not that the command merely finished.

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

/// Boot the deployed devcontainer exactly as the browser workbench does: the
/// CC-22 BusyBox rootfs (with the persistent `DEVCONTAINER_INIT`) over virtio-blk
/// + the shared virtio-9p workspace, on the CC-14 kernel.
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

fn console_since(emu: &Emulator, from: usize) -> String {
    String::from_utf8_lossy(&emu.console()[from..]).into_owned()
}

#[test]
#[ignore = "boots the deployed devcontainer + drives the terminal (~release) — run by the CC-11 vv suite"]
fn the_deployed_terminal_echoes_edits_and_handles_ctrl_c() {
    let mut emu = boot_deployed_devcontainer();
    emu.run(1_500_000_000);
    assert!(
        emu.console()
            .windows(b"holospace devcontainer ready".len())
            .any(|w| w == b"holospace devcontainer ready"),
        "the devcontainer booted to its interactive shell"
    );

    // 1) Raw echo + line editing: type `echo abZ`, backspace the `Z`, then `c`,
    //    then Enter. The backspace edits the line *in the guest tty*, so the
    //    command that runs is `echo abc`.
    let m = emu.console().len();
    emu.feed_console(b"echo abZ");
    emu.run(200_000_000);
    emu.feed_console(b"\x7f"); // backspace
    emu.run(100_000_000);
    emu.feed_console(b"c\n");
    emu.run(400_000_000);
    let seen = console_since(&emu, m);
    assert!(
        seen.contains("echo abZ"),
        "the typed keystrokes were echoed by the guest tty (raw input reached the OS); saw:\n{seen:?}"
    );
    assert!(
        seen.lines().any(|l| l.trim() == "abc"),
        "the backspace edited the line in the guest, so `echo abc` ran (output `abc`); saw:\n{seen:?}"
    );
    assert!(
        !seen.contains("abZ\r\nabZ") && !seen.lines().any(|l| l.trim() == "abZc"),
        "the `Z` was erased, not left in the command; saw:\n{seen:?}"
    );

    // 2) Ctrl-C → SIGINT: start a command that would block far past the budget,
    //    interrupt it, and confirm the shell came back to run the next command.
    let m2 = emu.console().len();
    emu.feed_console(b"sleep 999999\n");
    emu.run(300_000_000); // the guest is now blocked in sleep
    emu.feed_console(b"\x03"); // Ctrl-C
    emu.run(300_000_000);
    emu.feed_console(b"echo AFTER-CTRLC\n");
    emu.run(400_000_000);
    let seen = console_since(&emu, m2);
    assert!(
        seen.lines().any(|l| l.trim() == "AFTER-CTRLC"),
        "Ctrl-C interrupted `sleep 999999` (SIGINT reached the foreground process), so the \
         shell returned to a prompt and ran the next command; saw:\n{seen:?}"
    );
}
