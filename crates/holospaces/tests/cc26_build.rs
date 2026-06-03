//! `CC-26` — a devcontainer declared with a **Dockerfile build** is built on the
//! substrate, no Docker daemon.
//!
//! A `devcontainer.json` may declare its container as a Dockerfile build
//! (`"build": { "dockerfile": "Dockerfile" }`) instead of a prebuilt `image`.
//! holospaces honours it the substrate-native way: it parses the Dockerfile, its
//! `FROM` image is pulled + assembled as the base rootfs (`CC-20`/`CC-10`), the
//! `COPY` sources from the build context are injected into the rootfs, and the
//! `RUN` instructions run **in the devcontainer OS** during the build phase — with
//! the build `ARG`s and `ENV`s in scope — exactly as `docker build` runs them,
//! only on the emulator over the substrate (`CC-22`/`CC-25` machinery). The result
//! is the built rootfs.
//!
//! The external authority is the **Dockerfile reference** (the instruction
//! semantics: `FROM`/`ENV`/`WORKDIR`/`COPY`/`RUN`) + the Dev Container spec's
//! `build`, and the **`ext4` on-disk format** (e2fsprogs as the oracle). The OS
//! *running* the build is witnessed on holospaces' own emulator and,
//! differentially, on `qemu-system-riscv64`.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_with_files, Layer};
use holospaces::dockerfile;
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};

// A devcontainer Dockerfile build: FROM the busybox base, an ENV the RUN uses, a
// WORKDIR, a COPY of a script from the build context, and RUN steps (one runs the
// copied script, one echoes a marker with the ENV). All busybox-runnable.
const DOCKERFILE: &[u8] = br#"
# a devcontainer Dockerfile (CC-26)
ARG TAG=latest
FROM holospaces/busybox:${TAG}
ENV BUILT_BY=cc26 LANG=C.UTF-8
WORKDIR /workspace
COPY setup.sh /usr/local/bin/setup.sh
RUN busybox sh /usr/local/bin/setup.sh
RUN echo BUILD-RAN:$BUILT_BY in $(pwd)
"#;
// The build context (the `COPY` source).
const SETUP_SH: &[u8] =
    b"#!/bin/sh\necho SETUP-RAN\nmkdir -p /opt/built\necho ok > /opt/built/marker\n";

fn art() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts")
}
fn ingest_busybox(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let dir = art().join("cc22/image");
    let layout = std::fs::read(dir.join("oci-layout")).unwrap();
    let index = std::fs::read(dir.join("index.json")).unwrap();
    let blob = move |digest: &str| -> Option<Vec<u8>> {
        let hex = digest.strip_prefix("sha256:")?;
        std::fs::read(dir.join("blobs/sha256").join(hex)).ok()
    };
    ingest_image(store, &layout, &index, holospaces::Arch::Riscv64, blob)
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

/// Parse the Dockerfile, resolve `FROM` (the busybox base), inject the `COPY`
/// sources from the (synthesized) build context, and the build `/init` that runs
/// the `RUN` steps in the OS — the substrate-native build of this devcontainer.
fn build_rootfs(store: &MemKappaStore) -> (dockerfile::Dockerfile, Vec<u8>) {
    let args = BTreeMap::new();
    let df = dockerfile::parse(DOCKERFILE, &args).expect("parse the Dockerfile");
    // FROM resolves to the busybox base (the ${TAG} ARG defaulted to "latest").
    assert_eq!(
        df.from, "holospaces/busybox:latest",
        "FROM resolved with the ARG default"
    );

    let base = ingest_busybox(store).expect("pull/ingest the FROM base image");
    let owned: Vec<(String, Vec<u8>)> = base
        .layers()
        .iter()
        .zip(base.layer_media_types())
        .map(|(k, mt)| (mt.clone(), store.get(k).unwrap().unwrap().as_ref().to_vec()))
        .collect();
    let layers: Vec<Layer> = owned
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();

    // The build context: resolve each COPY src → its bytes (here, the synthesized
    // setup.sh). A real import reads them from the repository archive.
    let context = |src: &str| -> Vec<u8> {
        assert_eq!(
            src, "setup.sh",
            "the COPY source comes from the build context"
        );
        SETUP_SH.to_vec()
    };

    let init = df.build_init(None);
    let mut files: Vec<(String, u16, Vec<u8>)> = vec![("init".into(), 0o755, init)];
    for (src, dst) in df.copies() {
        files.push((dst.trim_start_matches('/').to_owned(), 0o755, context(src)));
    }
    let f: Vec<(&str, u16, &[u8])> = files
        .iter()
        .map(|(p, m, b)| (p.as_str(), *m, b.as_slice()))
        .collect();
    let rootfs = assemble_ext4_with_files(&layers, &f).expect("assemble the built rootfs");
    (df, rootfs)
}

/// (1) The Dockerfile is parsed and *honoured*: the build `/init` runs the `RUN`
/// steps with the `ENV` in scope, the `COPY` source is injected into the rootfs at
/// its destination, and `FROM` is the pulled base — verified against the `ext4`
/// format by e2fsprogs.
#[test]
fn the_dockerfile_build_is_assembled() {
    let store = MemKappaStore::new();
    let (df, rootfs) = build_rootfs(&store);

    let init = String::from_utf8(df.build_init(None)).unwrap();
    assert!(
        init.contains("export BUILT_BY='cc26'"),
        "ENV exported: {init}"
    );
    assert!(init.contains("cd '/workspace'"), "WORKDIR entered: {init}");
    assert!(
        init.contains("busybox sh /usr/local/bin/setup.sh"),
        "the COPY'd script runs: {init}"
    );
    assert!(
        init.contains("echo BUILD-RAN:$BUILT_BY"),
        "the RUN uses the ENV: {init}"
    );

    if !have("e2fsck") || !have("debugfs") {
        eprintln!("SKIP: e2fsprogs not available");
        return;
    }
    let img = std::env::temp_dir().join(format!("cc26-{}.img", std::process::id()));
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
    let mut c = Command::new("debugfs")
        .args(["-R", "cat /usr/local/bin/setup.sh", img.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut o = Vec::new();
    c.stdout.take().unwrap().read_to_end(&mut o).unwrap();
    c.wait().unwrap();
    let _ = std::fs::remove_file(&img);
    assert!(
        String::from_utf8_lossy(&o).contains("SETUP-RAN"),
        "the COPY source from the build context is in the rootfs (the RUN will execute it)"
    );
}

/// (2) holospaces' **own emulator** runs the Dockerfile build in the OS: the `RUN`
/// steps execute (the `COPY`'d script runs, the `ENV` applies) — `SETUP-RAN` and
/// `BUILD-RAN:cc26` appear between `BUILD-START`/`BUILD-DONE`, no Docker daemon.
#[test]
#[ignore]
fn the_emulator_runs_the_build() {
    let store = MemKappaStore::new();
    let (_df, rootfs) = build_rootfs(&store);
    let kernel = gunzip_cc14_kernel();
    let mut emu = MachineSpec::devcontainer()
        .boot(&kernel, rootfs)
        .expect("boot");
    emu.run(1_500_000_000);
    let c = String::from_utf8_lossy(emu.console()).into_owned();
    assert!(
        c.contains("BUILD-START") && c.contains("BUILD-DONE"),
        "the build ran; console:\n{c}"
    );
    assert!(
        c.contains("SETUP-RAN"),
        "the COPY'd script ran (COPY + RUN); console:\n{c}"
    );
    assert!(
        c.contains("BUILD-RAN:cc26"),
        "the RUN used the ENV (BUILT_BY=cc26); console:\n{c}"
    );
}

/// (3) The differential oracle: `qemu-system-riscv64` boots the byte-identical
/// built rootfs and produces the same build markers.
#[test]
#[ignore]
fn qemu_runs_the_build() {
    if !have("qemu-system-riscv64") {
        eprintln!("SKIP: qemu-system-riscv64 not available");
        return;
    }
    let store = MemKappaStore::new();
    let (_df, rootfs) = build_rootfs(&store);
    let tmp = std::env::temp_dir();
    let img = tmp.join(format!("cc26-qemu-{}.img", std::process::id()));
    std::fs::write(&img, &rootfs).unwrap();
    let kernel = tmp.join(format!("cc26-qemu-{}.kernel", std::process::id()));
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
    let c = String::from_utf8_lossy(&out.stdout);
    assert!(
        c.contains("SETUP-RAN") && c.contains("BUILD-RAN:cc26"),
        "QEMU ran the Dockerfile build; console:\n{c}"
    );
}

fn gunzip_cc14_kernel() -> Vec<u8> {
    let raw = std::fs::read(art().join("cc14/kernel/Image.gz")).unwrap();
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut k = Vec::new();
    d.read_to_end(&mut k).unwrap();
    k
}

/// (4) The *import* flow honours a Dockerfile build (it does not silently fall
/// back to the default image): from a repository archive declaring `build`,
/// holospaces finds the `devcontainer.json`, parses it to a `Build` source
/// (retaining the Dockerfile path + args), reads the Dockerfile from the build
/// context, resolves its `FROM` + its `COPY` sources from the repository — the
/// resolution the import does before pulling `FROM` and assembling the build.
#[test]
fn the_import_resolves_a_dockerfile_build_from_a_repo() {
    use holospaces::assembly::{find_devcontainer_json, read_archive_file};
    use holospaces::boot::devcontainer::{self, ImageSource};

    let archive = std::fs::read(art().join("cc26/repo.tar.gz")).unwrap();
    let layer = Layer {
        media_type: "application/gzip",
        blob: &archive,
    };

    // The import finds the devcontainer.json (the leading `<repo>-<ref>/` dir is
    // stripped) and parses it — `build` retained, not dropped.
    let cfg = find_devcontainer_json(&layer)
        .unwrap()
        .expect("devcontainer.json in the repo");
    let dc = devcontainer::parse(&cfg).expect("parse the devcontainer");
    let bc = match &dc.image_source {
        ImageSource::Build(bc) => bc,
        other => panic!("expected a Build image source, got {other:?}"),
    };
    assert_eq!(bc.dockerfile, "Dockerfile");
    assert_eq!(bc.args.get("TAG").map(String::as_str), Some("latest"));

    // The Dockerfile is read from the build context and resolves its FROM.
    let df_bytes = read_archive_file(&layer, ".devcontainer/Dockerfile")
        .unwrap()
        .expect("the build's Dockerfile is read from the repository");
    let df = dockerfile::parse(&df_bytes, &bc.args).expect("parse the Dockerfile");
    assert_eq!(
        df.from, "holospaces/busybox:latest",
        "FROM is the image to pull"
    );

    // Every COPY source is resolved from the build context (never dropped).
    for (src, _dst) in df.copies() {
        let resolved = read_archive_file(&layer, &format!(".devcontainer/{src}")).unwrap();
        assert!(
            resolved.is_some(),
            "the COPY source `{src}` resolves from the repository"
        );
    }
}
