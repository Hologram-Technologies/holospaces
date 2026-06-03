//! `CC-22` — the devcontainer's lifecycle commands run on create, so the
//! environment is ready on entry.
//!
//! A Codespace/Gitpod runs the Dev Container lifecycle commands
//! (`postCreateCommand`, …) in the environment so dependencies are installed and
//! the workspace is ready when you open it. holospaces realizes this: it parses
//! the commands from `devcontainer.json` (`CC-4`), and the Boot Orchestrator
//! **builds an `/init` from the parsed config** ([`DevContainer::lifecycle_init`])
//! and **injects it into the assembled rootfs**
//! ([`assemble_ext4_with_init`](holospaces::assembly::assemble_ext4_with_init)),
//! over a base image that provides a shell — so the booted OS runs the declared
//! commands in spec order, with the config's `remoteEnv` applied.
//!
//! The external authority is the **Dev Container specification** (the lifecycle
//! hooks + their run order) and the **`ext4` on-disk format** (e2fsprogs
//! `e2fsck`/`debugfs` as the oracle: the injected `/init` is present and exact in
//! the assembled rootfs). The booted OS *executing* the lifecycle commands under
//! a real **libc** shell (`busybox`) is witnessed two ways: on **holospaces' own
//! emulator** (the substrate the holospace actually runs on) and, differentially,
//! on `qemu-system-riscv64` (the reference RISC-V machine) — the same rootfs
//! produces the same lifecycle markers on both.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4_bootable, assemble_ext4_with_init, Layer};
use holospaces::boot::devcontainer::{self, LifecycleHook};
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};

/// Assemble the CC-22 rootfs: the busybox base image's layers with the
/// config-derived lifecycle `/init` injected (the rootfs the OS boots).
fn assemble_rootfs(store: &MemKappaStore, init: &[u8]) -> Vec<u8> {
    let img = ingest(store).expect("ingest the CC-22 busybox image");
    let layers_owned: Vec<(String, Vec<u8>)> = img
        .layers()
        .iter()
        .zip(img.layer_media_types())
        .map(|(k, mt)| (mt.clone(), store.get(k).unwrap().unwrap().as_ref().to_vec()))
        .collect();
    let layers: Vec<Layer> = layers_owned
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();
    assemble_ext4_with_init(&layers, init).expect("assemble")
}

/// Assemble the CC-22 busybox rootfs as a **bootable, writable** devcontainer disk
/// of `disk_bytes` (the deployed path) — `init` injected, free space for the guest.
fn assemble_bootable_rootfs(store: &MemKappaStore, init: &[u8], disk_bytes: u64) -> Vec<u8> {
    let img = ingest(store).expect("ingest the CC-22 busybox image");
    let layers_owned: Vec<(String, Vec<u8>)> = img
        .layers()
        .iter()
        .zip(img.layer_media_types())
        .map(|(k, mt)| (mt.clone(), store.get(k).unwrap().unwrap().as_ref().to_vec()))
        .collect();
    let layers: Vec<Layer> = layers_owned
        .iter()
        .map(|(mt, b)| Layer {
            media_type: mt,
            blob: b,
        })
        .collect();
    assemble_ext4_bootable(&layers, init, disk_bytes).expect("assemble bootable")
}

fn cc22_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc22")
}

fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(cc22_dir().join("image/blobs/sha256").join(hex)).ok()
}
fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(cc22_dir().join("image/oci-layout")).unwrap();
    let index = std::fs::read(cc22_dir().join("image/index.json")).unwrap();
    ingest_image(
        store,
        &layout,
        &index,
        holospaces::Arch::Riscv64,
        blob_bytes,
    )
}
/// Minimal USTAR reader: return the bytes of `want` (a file path in the tar).
fn extract_tar_file(tar: &[u8], want: &str) -> Option<Vec<u8>> {
    let mut off = 0;
    while off + 512 <= tar.len() {
        let hdr = &tar[off..off + 512];
        if hdr.iter().all(|&b| b == 0) {
            break;
        }
        let name_end = hdr[..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = String::from_utf8_lossy(&hdr[..name_end]).into_owned();
        let size_str = String::from_utf8_lossy(&hdr[124..136]);
        let size =
            usize::from_str_radix(size_str.trim_matches(|c| c == ' ' || c == '\0'), 8).unwrap_or(0);
        let data_off = off + 512;
        if name.trim_start_matches("./") == want {
            return Some(tar[data_off..data_off + size].to_vec());
        }
        off = data_off + size.div_ceil(512) * 512;
    }
    None
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

const CONFIG: &[u8] = br#"{
    "image": "holospaces/busybox",
    "remoteEnv": { "GREETING": "ready-on-entry" },
    "onCreateCommand": "echo CC22-ONCREATE",
    "postCreateCommand": "echo CC22-POSTCREATE:$GREETING",
    "postStartCommand": ["echo", "CC22-POSTSTART"]
}"#;

/// (1) The Boot Orchestrator builds the lifecycle `/init` from the parsed config:
/// the lifecycle commands in spec order, the `remoteEnv` applied, framed and
/// powered off — driven by the config, not hard-coded.
#[test]
fn the_lifecycle_init_is_built_from_the_parsed_config() {
    let dc = devcontainer::parse(CONFIG).expect("parse the devcontainer");
    // Parsed in spec order: onCreate before postCreate before postStart.
    assert_eq!(
        dc.lifecycle.iter().map(|(h, _)| *h).collect::<Vec<_>>(),
        vec![
            LifecycleHook::OnCreate,
            LifecycleHook::PostCreate,
            LifecycleHook::PostStart
        ],
    );
    let init = String::from_utf8(dc.lifecycle_init()).unwrap();
    assert!(
        init.starts_with("#!/bin/busybox sh\n"),
        "a shell init: {init}"
    );
    assert!(
        init.contains("export GREETING='ready-on-entry'"),
        "remoteEnv applied: {init}"
    );
    // The commands appear in spec order, each framed with its hook marker.
    let onc = init.find("CC22-ONCREATE").expect("onCreate present");
    let postc = init
        .find("CC22-POSTCREATE:$GREETING")
        .expect("postCreate present");
    let posts = init.find("CC22-POSTSTART").expect("postStart present");
    assert!(
        onc < postc && postc < posts,
        "lifecycle commands in spec order: {init}"
    );
    assert!(
        init.contains("HOOK:postCreateCommand"),
        "hooks are marked: {init}"
    );
    assert!(
        init.contains("busybox reboot -f"),
        "powers off when done: {init}"
    );
}

/// Both `containerEnv` and `remoteEnv` are honoured in the lifecycle environment,
/// `remoteEnv` layered over `containerEnv` (the Dev Container spec's precedence) —
/// neither parsed-and-dropped.
#[test]
fn container_env_and_remote_env_are_both_honoured() {
    const CFG: &[u8] = br#"{
        "image": "holospaces/busybox",
        "containerEnv": { "TOOL_HOME": "/opt/tool", "GREETING": "from-container" },
        "remoteEnv": { "GREETING": "from-remote" },
        "postCreateCommand": "echo $TOOL_HOME:$GREETING"
    }"#;
    let dc = devcontainer::parse(CFG).expect("parse");
    assert_eq!(dc.container_env.get("TOOL_HOME").unwrap(), "/opt/tool");
    let init = String::from_utf8(dc.lifecycle_init()).unwrap();
    // containerEnv is exported (honoured, not dropped)…
    assert!(
        init.contains("export TOOL_HOME='/opt/tool'"),
        "containerEnv applied: {init}"
    );
    // …and remoteEnv layers over it: its GREETING export comes *after* the
    // containerEnv one, so the shell's last-wins gives remoteEnv precedence.
    let container_g = init
        .find("export GREETING='from-container'")
        .expect("containerEnv GREETING present");
    let remote_g = init
        .find("export GREETING='from-remote'")
        .expect("remoteEnv GREETING present");
    assert!(
        container_g < remote_g,
        "remoteEnv layers over containerEnv (spec precedence): {init}"
    );
}

/// (3) The **differential oracle**: `qemu-system-riscv64` (the reference RISC-V
/// machine, as in `CC-9`/`CC-14`/`CC-16`) boots the byte-identical holospaces
/// rootfs and runs the injected lifecycle `/init`, producing the same markers as
/// holospaces' own emulator (test 4) — the lifecycle realization is correct
/// independent of the emulator. Heavy + needs QEMU, so `#[ignore]`d.
#[test]
#[ignore]
fn the_os_runs_the_devcontainer_lifecycle_commands() {
    if !have("qemu-system-riscv64") {
        eprintln!("SKIP: qemu-system-riscv64 not available");
        return;
    }
    let dc = devcontainer::parse(CONFIG).expect("parse");
    let init = dc.lifecycle_init();
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the CC-22 busybox image");
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
    let rootfs = assemble_ext4_with_init(&layers, &init).expect("assemble");

    let tmp = std::env::temp_dir();
    let img_path = tmp.join(format!("cc22-qemu-{}.img", std::process::id()));
    std::fs::write(&img_path, &rootfs).unwrap();
    // The CC-14 kernel (gunzipped for QEMU's -kernel).
    let kernel_gz =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14/kernel/Image.gz");
    let kernel = tmp.join(format!("cc22-qemu-{}.kernel", std::process::id()));
    {
        let raw = std::fs::read(&kernel_gz).unwrap();
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut k = Vec::new();
        d.read_to_end(&mut k).unwrap();
        std::fs::write(&kernel, &k).unwrap();
    }

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
            &format!("file={},format=raw,if=none,id=hd0", img_path.display()),
            "-device",
            "virtio-blk-device,drive=hd0",
            "-append",
            "root=/dev/vda rw console=hvc0 earlycon=sbi init=/init",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("run qemu");
    let _ = std::fs::remove_file(&img_path);
    let _ = std::fs::remove_file(&kernel);
    let console = String::from_utf8_lossy(&out.stdout);

    assert!(
        console.contains("LIFECYCLE-START") && console.contains("LIFECYCLE-DONE"),
        "the OS ran the lifecycle runner from the holospaces rootfs; console:\n{console}"
    );
    assert!(
        console.contains("HOOK:postCreateCommand"),
        "the postCreateCommand hook ran in spec order; console:\n{console}"
    );
    assert!(
        console.contains("CC22-POSTCREATE:ready-on-entry"),
        "the OS ran the declared postCreateCommand, with the config's remoteEnv applied — the environment is ready on entry (CC-22); console:\n{console}"
    );
    assert!(
        console.contains("CC22-POSTSTART"),
        "the postStartCommand ran too (lifecycle order); console:\n{console}"
    );
}

/// (4) holospaces' **own emulator** runs the lifecycle commands — the substrate
/// the holospace actually boots on, with a real **libc** (`busybox`) userland.
/// The emulator boots the holospaces-assembled rootfs, execs the injected
/// lifecycle `/init`, and the declared `postCreateCommand` output appears on its
/// console with `remoteEnv` applied — the environment is ready on entry, no QEMU
/// in the loop. Heavy (a real-OS boot to userland), so `#[ignore]`d.
#[test]
#[ignore]
fn the_holospaces_emulator_runs_the_lifecycle() {
    let dc = devcontainer::parse(CONFIG).expect("parse");
    let init = dc.lifecycle_init();
    let store = MemKappaStore::new();
    let rootfs = assemble_rootfs(&store, &init);

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
        console.contains("LIFECYCLE-START") && console.contains("LIFECYCLE-DONE"),
        "the holospaces emulator ran the lifecycle runner from the assembled rootfs; console:\n{console}"
    );
    assert!(
        console.contains("HOOK:postCreateCommand"),
        "the postCreateCommand hook ran in spec order; console:\n{console}"
    );
    assert!(
        console.contains("CC22-POSTCREATE:ready-on-entry"),
        "the holospaces emulator ran the declared postCreateCommand under a real libc shell, with remoteEnv applied — ready on entry (CC-22); console:\n{console}"
    );
    assert!(
        console.contains("CC22-POSTSTART"),
        "the postStartCommand ran too (lifecycle order); console:\n{console}"
    );
}

/// The **deployed** devcontainer boots as a *running* dev environment — not the
/// boot-and-halt conformance init. The persistent `/init`
/// ([`machine::DEVCONTAINER_INIT`]) mounts the pseudo filesystems and the shared
/// `virtio-9p` workspace and execs an interactive shell, so the OS **stays up**
/// (it does not power off after boot), the shell answers commands, and a file the
/// holospace places on the workspace is readable at `/workspace`. Also asserts the
/// machine advertises **no unattached `virtio-mmio` slot** (the empty network slot
/// is gone), so the guest does not log "Wrong magic value" and stall. Heavy (a
/// real-OS boot to an interactive shell), so `#[ignore]`d.
#[test]
#[ignore]
fn the_deployed_devcontainer_boots_a_persistent_interactive_shell() {
    use holospaces::machine::DEVCONTAINER_INIT;
    let store = MemKappaStore::new();
    // A bootable, writable disk (as the deployed workbench assembles): room for the
    // init to mount /workspace, install BusyBox applets, and the user to work.
    let rootfs = assemble_bootable_rootfs(&store, DEVCONTAINER_INIT, 64 * 1024 * 1024);

    let kernel_gz =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14/kernel/Image.gz");
    let kernel = {
        let raw = std::fs::read(&kernel_gz).unwrap();
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut k = Vec::new();
        d.read_to_end(&mut k).unwrap();
        k
    };

    // Boot as the deployed workbench does: the rootfs over virtio-blk + the shared
    // virtio-9p workspace (seeded as the holospace seeds WELCOME.md), no network.
    let mut emu = MachineSpec::devcontainer()
        .boot_workspace(
            &kernel,
            rootfs,
            &[("WELCOME.md", b"hello from the holospace")],
        )
        .expect("boot the persistent devcontainer");
    emu.run(1_500_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();

    assert!(
        console.contains("holospace devcontainer ready"),
        "the persistent init ran (proc/sys/dev + 9p mounted, shell about to start); console:\n{console}"
    );
    assert!(
        !console.contains("reboot: Power down") && !console.contains("Power off"),
        "the OS STAYS UP — it does not power off after boot (a running dev environment); console:\n{console}"
    );
    assert!(
        !console.contains("Wrong magic value"),
        "no unattached virtio-mmio slot is advertised (the empty network node is gone); console:\n{console}"
    );

    // The shell is interactive and the workspace is mounted: run a command that
    // reads the file the holospace placed on the shared 9p workspace.
    emu.feed_console(b"cat /workspace/WELCOME.md\n");
    emu.run(800_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();
    assert!(
        console.contains("hello from the holospace"),
        "the interactive shell read the file from the mounted 9p workspace; console:\n{console}"
    );
}

/// (2) The lifecycle `/init` is injected into the assembled rootfs: the booted OS
/// finds it at `/init` with its exact bytes — verified against the **ext4 format**
/// by e2fsprogs (`e2fsck` finds the image clean; `debugfs` reads `/init` back
/// byte-identically). This is the rootfs the OS boots to run the lifecycle.
#[test]
fn the_lifecycle_init_is_injected_into_the_assembled_rootfs() {
    if !have("e2fsck") || !have("debugfs") {
        eprintln!("SKIP: e2fsprogs (e2fsck/debugfs) not available");
        return;
    }
    let dc = devcontainer::parse(CONFIG).expect("parse");
    let init = dc.lifecycle_init();

    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the CC-22 busybox image");
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
    let rootfs = assemble_ext4_with_init(&layers, &init).expect("assemble with the lifecycle init");
    assert!(
        rootfs.len().is_multiple_of(4096) && !rootfs.is_empty(),
        "a whole-block ext4 image"
    );

    let dir = std::env::temp_dir().join(format!("cc22-rootfs-{}.img", std::process::id()));
    std::fs::write(&dir, &rootfs).unwrap();

    // Oracle 1: e2fsck finds the assembled ext4 structurally clean.
    let fsck = Command::new("e2fsck")
        .args(["-fn", dir.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        fsck.status.code() == Some(0),
        "e2fsck must find the assembled ext4 clean:\n{}",
        String::from_utf8_lossy(&fsck.stdout)
    );

    // Oracle 2: debugfs reads /init back — byte-identical to what we injected.
    let mut child = Command::new("debugfs")
        .args(["-R", "cat /init", dir.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut got = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut got).unwrap();
    child.wait().unwrap();
    assert_eq!(
        got, init,
        "debugfs reads the injected lifecycle /init back byte-identically — the OS boots this"
    );

    // The base image's large binary (/bin/busybox, ~1 MB, a multi-extent file)
    // must also read back byte-identically — the rootfs the OS executes.
    let mut child = Command::new("debugfs")
        .args(["-R", "cat /bin/busybox", dir.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut bb = Vec::new();
    child.stdout.take().unwrap().read_to_end(&mut bb).unwrap();
    child.wait().unwrap();
    let _ = std::fs::remove_file(&dir);
    // The expected busybox bytes (from the layer we assembled).
    let expected = {
        // decompress the single layer tar.gz and find bin/busybox
        let raw = &blobs[0].1;
        let mut d = flate2::read::GzDecoder::new(&raw[..]);
        let mut tar = Vec::new();
        d.read_to_end(&mut tar).unwrap();
        extract_tar_file(&tar, "bin/busybox").expect("busybox in the layer")
    };
    assert_eq!(
        bb.len(),
        expected.len(),
        "debugfs reads /bin/busybox back at the right length ({} vs {})",
        bb.len(),
        expected.len()
    );
    assert_eq!(
        bb, expected,
        "the large multi-extent /bin/busybox reads back byte-identically"
    );

    // The injected /init is also a valid busybox shell program (sanity: it would
    // run the declared commands once the OS executes it).
    assert!(
        got.starts_with(b"#!/bin/busybox sh")
            && got
                .windows(13)
                .any(|w| w == b"CC22-POSTCREATE"[..13].as_ref()),
        "the injected /init is the lifecycle runner"
    );
    let _ = std::io::stdout().flush();
}
