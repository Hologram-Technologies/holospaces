//! `CC-25` — the devcontainer's **Dev Container Features** are installed on
//! create, so feature-provided tools are present on entry.
//!
//! A Codespace/Gitpod installs the `features` a `devcontainer.json` declares —
//! each a published OCI artifact (a `devcontainer-feature.json` + an `install.sh`)
//! whose script runs in the container during the build, before the lifecycle
//! commands. holospaces realizes this: it parses `features` (`CC-4`), the Boot
//! Orchestrator **imports each feature artifact by κ** (verify-by-re-derivation,
//! the `CC-20` machinery) and **places it into the rootfs**, and the generated
//! `/init` runs each feature's `install.sh` *before* the lifecycle commands
//! (`CC-22`), with the declared options passed as uppercased environment — exactly
//! as a Codespace installs features (ADR-016).
//!
//! The external authority is the **Dev Container Features specification** (the
//! feature artifact format + the install contract: `install.sh` receives options
//! as environment) and the **`ext4` on-disk format** (e2fsprogs as the oracle that
//! the feature is present in the assembled rootfs). The OS *running* the feature's
//! `install.sh` is witnessed on holospaces' own emulator and, differentially, on
//! `qemu-system-riscv64`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_with_files, extract_layer_files, Layer};
use holospaces::boot::devcontainer;
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};

fn art() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts")
}
fn ingest_at(store: &MemKappaStore, image_dir: &Path) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(image_dir.join("oci-layout")).unwrap();
    let index = std::fs::read(image_dir.join("index.json")).unwrap();
    let blob = |digest: &str| -> Option<Vec<u8>> {
        let hex = digest.strip_prefix("sha256:")?;
        std::fs::read(image_dir.join("blobs/sha256").join(hex)).ok()
    };
    ingest_image(store, &layout, &index, holospaces::Arch::Riscv64, blob)
}
fn layers_of<'a>(
    store: &MemKappaStore,
    img: &IngestedImage,
    out: &'a mut Vec<(String, Vec<u8>)>,
) -> Vec<Layer<'a>> {
    *out = img
        .layers()
        .iter()
        .zip(img.layer_media_types())
        .map(|(k, mt)| (mt.clone(), store.get(k).unwrap().unwrap().as_ref().to_vec()))
        .collect();
    out.iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect()
}
fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| true)
        .unwrap_or(false)
}

// A devcontainer that declares a feature (the CC-25 artifact) with an option, plus
// a postCreate command — so the witness shows features install BEFORE the lifecycle.
const CONFIG: &[u8] = br#"{
    "image": "holospaces/busybox",
    "features": { "ghcr.io/holospaces/features/holospace-demo:1": { "version": "2.0" } },
    "postCreateCommand": "echo CC25-POSTCREATE:after-features"
}"#;

/// The base busybox shell image (reused from CC-22), the feature OCI artifact
/// (CC-25), the parsed config, and the assembled rootfs: the feature's files are
/// placed at `/opt/holospaces/features/0/`, the lifecycle `/init` is generated to
/// run its `install.sh`, over the busybox base.
fn assemble(store: &MemKappaStore) -> (Vec<u8>, Vec<u8>) {
    let dc = devcontainer::parse(CONFIG).expect("parse the devcontainer (feature parsed)");
    assert_eq!(dc.features.len(), 1, "the feature is parsed");
    assert_eq!(
        dc.features[0].options.get("version").map(String::as_str),
        Some("2.0")
    );
    let init = dc.lifecycle_init();

    // Import the feature artifact by κ and unpack it (CC-20 ingest → its files).
    let feat_img =
        ingest_at(store, &art().join("cc25/feature")).expect("ingest the feature artifact");
    let mut fbuf = Vec::new();
    let feat_layers = layers_of(store, &feat_img, &mut fbuf);
    let feature_files = extract_layer_files(&feat_layers[0]).expect("unpack the feature");
    assert!(
        !feature_files.is_empty(),
        "the feature artifact unpacked to files"
    );

    // The busybox base (CC-22) provides the shell that runs install.sh.
    let base_img = ingest_at(store, &art().join("cc22/image")).expect("ingest the busybox base");
    let mut bbuf = Vec::new();
    let base_layers = layers_of(store, &base_img, &mut bbuf);

    // Inject /init + the feature's files at /opt/holospaces/features/0/.
    let mut owned: Vec<(String, u16, Vec<u8>)> = vec![("init".into(), 0o755, init.clone())];
    for (name, mode, bytes) in &feature_files {
        owned.push((
            format!("opt/holospaces/features/0/{name}"),
            *mode,
            bytes.clone(),
        ));
    }
    let files: Vec<(&str, u16, &[u8])> = owned
        .iter()
        .map(|(p, m, b)| (p.as_str(), *m, b.as_slice()))
        .collect();
    let rootfs = assemble_ext4_with_files(&base_layers, &files)
        .expect("assemble the rootfs with the feature");
    (init, rootfs)
}

/// (1) The feature is parsed and the generated `/init` is **wired** to run the
/// feature's `install.sh` (with the option as env, before the lifecycle — spec
/// order), and the feature's files are **staged** in the assembled `ext4` rootfs —
/// verified against the format by e2fsprogs (`e2fsck` clean; `debugfs` reads the
/// feature back). The actual `install.sh` *execution* is witnessed by the emulator
/// boot tests below, not here.
#[test]
fn the_feature_is_staged_and_scheduled_in_the_rootfs() {
    let store = MemKappaStore::new();
    let (init, rootfs) = assemble(&store);

    let inits = String::from_utf8(init).unwrap();
    assert!(
        inits.contains("echo FEATURES-START"),
        "the init installs features: {inits}"
    );
    assert!(
        inits.contains("ghcr.io/holospaces/features/holospace-demo:1"),
        "the feature is named in the init: {inits}"
    );
    assert!(
        inits.contains("VERSION='2.0'"),
        "the option is passed as uppercased env: {inits}"
    );
    assert!(
        inits.contains("busybox sh ./install.sh"),
        "the init is wired to run install.sh: {inits}"
    );
    let feat = inits.find("FEATURES-DONE").expect("features done");
    let life = inits.find("LIFECYCLE-START").expect("lifecycle start");
    assert!(
        feat < life,
        "features install BEFORE the lifecycle (spec order): {inits}"
    );

    if !have("e2fsck") || !have("debugfs") {
        eprintln!("SKIP: e2fsprogs not available");
        return;
    }
    let img = std::env::temp_dir().join(format!("cc25-{}.img", std::process::id()));
    std::fs::write(&img, &rootfs).unwrap();
    let fsck = Command::new("e2fsck")
        .args(["-fn", img.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        fsck.status.code() == Some(0),
        "e2fsck clean:\n{}",
        String::from_utf8_lossy(&fsck.stdout)
    );
    let cat = |p: &str| {
        let mut c = Command::new("debugfs")
            .args(["-R", &format!("cat {p}"), img.to_str().unwrap()])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut o = Vec::new();
        c.stdout.take().unwrap().read_to_end(&mut o).unwrap();
        c.wait().unwrap();
        o
    };
    let installed =
        String::from_utf8_lossy(&cat("/opt/holospaces/features/0/install.sh")).into_owned();
    let manifest =
        String::from_utf8_lossy(&cat("/opt/holospaces/features/0/devcontainer-feature.json"))
            .into_owned();
    let _ = std::fs::remove_file(&img);
    assert!(
        installed.contains("FEATURE-INSTALLED"),
        "debugfs reads the feature's install.sh out of the rootfs (the OS will run it): {installed:?}"
    );
    assert!(
        manifest.contains("holospace-demo"),
        "the feature's devcontainer-feature.json is in the rootfs too"
    );
}

/// (2) holospaces' **own emulator** runs the feature's `install.sh` in the OS,
/// before the lifecycle: `FEATURE-INSTALLED:2.0` (the option applied) appears, then
/// `CC25-POSTCREATE:after-features` — feature tools are present on entry, no QEMU.
#[test]
#[ignore]
fn the_emulator_installs_the_feature() {
    let store = MemKappaStore::new();
    let (_init, rootfs) = assemble(&store);
    let kernel = gunzip_cc14_kernel();
    let mut emu = MachineSpec::devcontainer()
        .boot(&kernel, rootfs)
        .expect("boot");
    emu.run(1_500_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();
    assert!(
        console.contains("FEATURES-START"),
        "features phase ran; console:\n{console}"
    );
    assert!(
        console.contains("FEATURE-INSTALLED:2.0"),
        "the feature's install.sh ran with the option (VERSION=2.0); console:\n{console}"
    );
    let fi = console
        .find("FEATURE-INSTALLED")
        .expect("feature installed");
    let pc = console.find("CC25-POSTCREATE").expect("postCreate ran");
    assert!(
        fi < pc,
        "the feature installed BEFORE the lifecycle command; console:\n{console}"
    );
}

/// (3) The differential oracle: `qemu-system-riscv64` boots the byte-identical
/// rootfs and produces the same feature-install markers — the realization is
/// correct independent of the emulator.
#[test]
#[ignore]
fn qemu_installs_the_feature() {
    if !have("qemu-system-riscv64") {
        eprintln!("SKIP: qemu-system-riscv64 not available");
        return;
    }
    let store = MemKappaStore::new();
    let (_init, rootfs) = assemble(&store);
    let tmp = std::env::temp_dir();
    let img = tmp.join(format!("cc25-qemu-{}.img", std::process::id()));
    std::fs::write(&img, &rootfs).unwrap();
    let kernel = tmp.join(format!("cc25-qemu-{}.kernel", std::process::id()));
    std::fs::write(&kernel, gunzip_cc14_kernel()).unwrap();
    let out = Command::new("qemu-system-riscv64")
        .args([
            "-M",
            "virt",
            "-m",
            "512M",
            "-nographic",
            "-no-reboot",
            "-bios",
            "default",
            "-global",
            "virtio-mmio.force-legacy=false",
            "-kernel",
            kernel.to_str().unwrap(),
            "-drive",
            &format!("file={},format=raw,if=none,id=hd0", img.display()),
            "-device",
            "virtio-blk-device,drive=hd0",
            "-append",
            "root=/dev/vda rw console=hvc0 earlycon=sbi init=/init",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("run qemu");
    let _ = std::fs::remove_file(&img);
    let _ = std::fs::remove_file(&kernel);
    let console = String::from_utf8_lossy(&out.stdout);
    assert!(
        console.contains("FEATURE-INSTALLED:2.0") && console.contains("CC25-POSTCREATE"),
        "QEMU ran the feature install + the lifecycle; console:\n{console}"
    );
}

fn gunzip_cc14_kernel() -> Vec<u8> {
    let raw = std::fs::read(art().join("cc14/kernel/Image.gz")).unwrap();
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut k = Vec::new();
    d.read_to_end(&mut k).unwrap();
    k
}
