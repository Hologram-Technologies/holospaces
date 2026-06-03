//! `CC-23` — the workspace is personalized: the operator's settings, dotfiles,
//! and secrets follow them into the devcontainer.
//!
//! A Codespace/Gitpod carries an operator's personalization so their environment
//! is ready wherever they sign in. holospaces realizes this *without a server
//! account*: a [`Personalization`](holospaces::personalization::Personalization)
//! is κ-addressed content that **embeds the operator identity** (`CC-1`/`CC-12`),
//! so it is held in the store and synced by the substrate (Laws L1/L3), and on
//! entry holospaces **applies** it — the dotfiles are injected into the
//! devcontainer OS's home directory and the secrets are exported into its
//! environment by an entry `/init`, the editor settings handed to the workbench.
//!
//! The external authorities are the **Dev Container spec** (`remoteEnv`/secrets
//! as environment), the **Codespaces/Gitpod dotfiles** convention (dotfiles in
//! `$HOME`), and the **`ext4` on-disk format** (e2fsprogs `e2fsck`/`debugfs` as
//! the oracle that the operator's dotfiles + entry runner are present and exact
//! in the assembled rootfs). The booted OS *applying* the personalization under a
//! real **libc** (`busybox`) shell is witnessed on holospaces' own emulator. The
//! shell base is the same published BusyBox as `CC-22` (`vv/artifacts/cc22`); the
//! personalization is the operator's own content layered over it.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio as PStdio};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_with_files, Layer};
use holospaces::identity::Operator;
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};
use holospaces::personalization::Personalization;

// The shell base is CC-22's published BusyBox image (reused, not duplicated).
fn shell_image_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc22/image")
}

fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(shell_image_dir().join("blobs/sha256").join(hex)).ok()
}

fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(shell_image_dir().join("oci-layout")).unwrap();
    let index = std::fs::read(shell_image_dir().join("index.json")).unwrap();
    ingest_image(
        store,
        &layout,
        &index,
        holospaces::Arch::Riscv64,
        blob_bytes,
    )
}

fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-V")
        .stdout(PStdio::null())
        .stderr(PStdio::null())
        .status()
        .map(|_| true)
        .unwrap_or(false)
}

const SETTINGS: &str = r#"{"editor.fontSize":15,"workbench.colorTheme":"Default Dark+"}"#;
const GITCONFIG: &[u8] = b"[user]\n\tname = Holospaces Operator\n\temail = op@uor.foundation\n";

/// The operator's personalization: editor settings, a `.gitconfig` dotfile, and a
/// `GH_TOKEN` secret — scoped to their self-sovereign identity.
fn operator_personalization() -> Personalization {
    let operator = Operator::from_public_key(b"cc23-operator-public-key");
    Personalization::new(&operator)
        .with_settings(SETTINGS)
        .with_dotfile(".gitconfig", GITCONFIG)
        .with_secret("GH_TOKEN", "gho_cc23_example_token")
}

/// Assemble the devcontainer rootfs with the personalization applied: the entry
/// `/init` (exports the secrets, confirms the dotfiles) at `/init`, and each
/// dotfile injected into the home directory (`/root/<name>`).
fn assemble_personalized(store: &MemKappaStore, p: &Personalization) -> Vec<u8> {
    let img = ingest(store).expect("ingest the busybox shell base");
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

    let init = p.entry_init();
    let home = p.home_files();
    let mut files: Vec<(&str, u16, &[u8])> = vec![("init", 0o755, init.as_slice())];
    for (path, content) in &home {
        files.push((path.as_str(), 0o644, content.as_slice()));
    }
    assemble_ext4_with_files(&layers, &files).expect("assemble the personalized rootfs")
}

/// (1) The operator's dotfiles + entry runner are injected into the assembled
/// rootfs: the booted OS finds the dotfile at `/root/.gitconfig` and the entry
/// runner at `/init`, each with its exact bytes — verified against the **ext4
/// format** by e2fsprogs (`e2fsck` finds the image clean; `debugfs` reads the
/// files back byte-identically). This is the rootfs the OS boots to apply the
/// personalization.
#[test]
fn the_operators_dotfiles_are_injected_into_the_assembled_rootfs() {
    if !have("e2fsck") || !have("debugfs") {
        eprintln!("SKIP: e2fsprogs (e2fsck/debugfs) not available");
        return;
    }
    let p = operator_personalization();
    let store = MemKappaStore::new();
    let rootfs = assemble_personalized(&store, &p);
    assert!(
        rootfs.len().is_multiple_of(4096) && !rootfs.is_empty(),
        "a whole-block ext4 image"
    );

    let img = std::env::temp_dir().join(format!("cc23-rootfs-{}.img", std::process::id()));
    std::fs::write(&img, &rootfs).unwrap();

    // Oracle 1: e2fsck finds the assembled ext4 structurally clean.
    let fsck = Command::new("e2fsck")
        .args(["-fn", img.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        fsck.status.code() == Some(0),
        "e2fsck must find the assembled ext4 clean:\n{}",
        String::from_utf8_lossy(&fsck.stdout)
    );

    // Oracle 2: debugfs reads the operator's dotfile back byte-identically — the
    // devcontainer OS carries the operator's .gitconfig.
    let got_dotfile = debugfs_cat(&img, "/root/.gitconfig");
    assert_eq!(
        got_dotfile, GITCONFIG,
        "debugfs reads the operator's .gitconfig back byte-identically from /root"
    );

    // The entry runner is present and exact, and is the personalization applier.
    let got_init = debugfs_cat(&img, "/init");
    let _ = std::fs::remove_file(&img);
    assert_eq!(
        got_init,
        p.entry_init(),
        "debugfs reads the entry /init back byte-identically — the OS boots this"
    );
    assert!(
        got_init.starts_with(b"#!/bin/busybox sh")
            && got_init.windows(8).any(|w| w == b"GH_TOKEN"[..8].as_ref()),
        "the injected /init applies the operator's secrets"
    );
}

/// (2) holospaces' **own emulator** applies the personalization in the booted OS,
/// under a real **libc** (`busybox`) shell: the emulator boots the assembled
/// rootfs, execs the entry `/init`, and the console shows the secret present in
/// the environment (without leaking its value) and the operator's dotfile in
/// place — the operator's environment is ready on entry, on this peer. Heavy (a
/// real-OS boot to userland), so `#[ignore]`d.
#[test]
#[ignore]
fn the_holospaces_emulator_applies_the_personalization() {
    let p = operator_personalization();
    let store = MemKappaStore::new();
    let rootfs = assemble_personalized(&store, &p);

    let kernel_gz =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14/kernel/Image.gz");
    let kernel = {
        let raw = std::fs::read(&kernel_gz).unwrap();
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut k = Vec::new();
        d.read_to_end(&mut k).unwrap();
        k
    };

    let mut emu = MachineSpec::devcontainer()
        .boot(&kernel, rootfs)
        .expect("boot the holospaces emulator");
    emu.run(1_500_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();

    assert!(
        console.contains("PERSONALIZATION-START") && console.contains("PERSONALIZATION-DONE"),
        "the holospaces emulator ran the entry runner from the assembled rootfs; console:\n{console}"
    );
    assert!(
        console.contains("SECRET-PRESENT:GH_TOKEN"),
        "the operator's secret is applied to the OS environment (present, value not leaked); console:\n{console}"
    );
    assert!(
        console.contains("DOTFILE-PRESENT:.gitconfig")
            && console.contains("name = Holospaces Operator"),
        "the operator's .gitconfig dotfile is in the devcontainer OS's home; console:\n{console}"
    );
    // The token value itself must not be required to appear — but the OS proved it
    // is in the environment. (We do not assert the raw token is on the console;
    // secrets are confirmed present, not printed.)
}

/// `debugfs -R "cat <path>"` — read a file out of the ext4 image.
fn debugfs_cat(img: &Path, path: &str) -> Vec<u8> {
    let mut child = Command::new("debugfs")
        .args(["-R", &format!("cat {path}"), img.to_str().unwrap()])
        .stdout(PStdio::piped())
        .stderr(PStdio::null())
        .spawn()
        .unwrap();
    let mut out = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut out).unwrap();
    child.wait().unwrap();
    out
}
