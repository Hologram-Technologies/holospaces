//! `CC-65` — any OCI image's REAL entrypoint runs on the x86-64 .holo core.
//!
//! The "run any docker image" promise needs the image's OWN `Entrypoint`/`Cmd` (with its `Env`,
//! `WorkingDir`, `User`) to run as PID 1 — not a hardcoded shell. This witness composes the existing
//! κ-native pipeline (ingest → assemble → boot) with the CC-65 keystone: a generic, libc-agnostic
//! freestanding init (`vv/artifacts/cc65/image-init`) into which the host patches the image's run
//! config, so the init mounts the pseudo-fs and `execve`s the app DIRECTLY (works for distroless too).
//!
//! Image A (this file): a real Alpine (musl) layer with `Cmd ["/bin/busybox","echo","HOLO-IMG-OK"]`
//! — proves config→init→boot→the-image's-actual-command-ran, fully over the κ-disk.

use std::io::Read;
use std::path::{Path, PathBuf};

use holospaces::assembly::{assemble_ext4_bootable, Layer};
use holospaces::emulator::x64::Cpu;
use holospaces::image_init::{image_init, run_config_from_oci, RunConfig};

fn art(p: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(p)
}
fn gunzip(p: PathBuf) -> Vec<u8> {
    let raw = std::fs::read(&p).unwrap_or_else(|_| panic!("read {}", p.display()));
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(&raw[..]).read_to_end(&mut out).unwrap();
    out
}

const CMDLINE: &str = "virtio_mmio.device=0x200@0xd0000000:11 console=ttyS0 root=/dev/vda rw \
                       init=/init random.trust_cpu=on norandmaps nmi_watchdog=0 nowatchdog tsc=reliable";

#[test]
fn image_a_alpine_cmd_runs_its_real_command() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let layer = std::fs::read(art("vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz")).unwrap();
    let template = std::fs::read(art("vv/artifacts/cc65/image-init"))
        .expect("compile vv/artifacts/cc65/image-init.c first (see its header)");

    // Image A's OCI image config, exactly as a registry would serve it — distilled through the SAME
    // run_config_from_oci path real images use (G2), then patched into the generic init (the keystone).
    let oci_config = br#"{
        "architecture":"amd64","os":"linux",
        "config":{
            "Cmd":["/bin/busybox","echo","HOLO-IMG-OK"],
            "Env":["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "WorkingDir":"/"
        }
    }"#;
    let cfg: RunConfig = run_config_from_oci(oci_config).expect("parse image A OCI config");
    assert_eq!(cfg.argv, ["/bin/busybox", "echo", "HOLO-IMG-OK"], "config→argv");
    let init = image_init(&template, &cfg).expect("patch image-init template with run config");

    let rootfs = assemble_ext4_bootable(
        &[Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer }],
        &init,
        256 * 1024 * 1024,
    )
    .expect("assemble bootable ext4 with the image-config init");

    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, rootfs, CMDLINE);
    let mut reached = false;
    for _ in 0..60 {
        cpu.run(20_000_000);
        let con = String::from_utf8_lossy(cpu.console());
        if con.contains("HOLO-IMG-OK") {
            reached = true;
            break;
        }
        if con.contains("System halted") && con.contains("HOLO-IMG-OK") {
            reached = true;
            break;
        }
    }
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let tail: Vec<_> = con.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
    assert!(
        reached,
        "image A's Cmd (busybox echo HOLO-IMG-OK) never ran via the generated init.\n  tail:\n  {}",
        tail.join("\n  ")
    );
    eprintln!("CC-65 image A: the image's own Cmd ran on x64 via image-init — saw HOLO-IMG-OK. insns={}", cpu.insns());
}

/// Image B — a real LONG-RUNNING SERVER from an image's config runs and serves real content over real
/// TCP. The image's Cmd brings up loopback, stages an HTTP/1.0 response, runs a persistent `nc` server
/// (the same TCP server surface CC-63 proved), then a real `wget` client fetches it — the body comes back
/// byte-exact. Proves "docker run <a server>": the image's app binds a port and serves, all over the
/// κ-disk via the generated init. (busybox here has no httpd applet, so the server is nc, like CC-63.)
#[test]
fn image_b_server_runs_and_serves_its_real_content() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let layer = std::fs::read(art("vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz")).unwrap();
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).expect("compile image-init.c first");

    // The image's entrypoint: a real server that serves a real page (verified in-guest over loopback).
    let script = r#"/bin/busybox ip link set lo up 2>/dev/null
printf 'HTTP/1.0 200 OK\r\nContent-Length: 21\r\n\r\nHELLO-FROM-HOLO-NGINX' > /resp
( while true; do /bin/busybox nc -l -p 8080 < /resp >/dev/null 2>&1; done ) &
/bin/busybox sleep 1
wget -qO- http://127.0.0.1:8080/
echo SERVED-OK
/bin/busybox sleep 1000000
"#;
    let cfg = RunConfig {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()],
        workdir: "/".into(),
        uid: 0,
        gid: 0,
        net_up: false,
    };
    let init = image_init(&template, &cfg).expect("patch image-init with the server config");
    let rootfs = assemble_ext4_bootable(
        &[Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer }],
        &init,
        256 * 1024 * 1024,
    )
    .expect("assemble bootable ext4 (server image)");

    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, rootfs, CMDLINE);
    let mut served = false;
    for _ in 0..200 {
        cpu.run(20_000_000);
        let con = String::from_utf8_lossy(cpu.console());
        if con.contains("HELLO-FROM-HOLO-NGINX") && con.contains("SERVED-OK") {
            served = true;
            break;
        }
        if con.contains("System halted") {
            break;
        }
    }
    let con = String::from_utf8_lossy(cpu.console()).into_owned();
    let tail: Vec<_> = con.lines().rev().take(10).collect::<Vec<_>>().into_iter().rev().collect();
    assert!(
        served,
        "image B's server (nc) never served its page to the in-guest client.\n  tail:\n  {}",
        tail.join("\n  ")
    );
    eprintln!("CC-65 image B: the image's server app ran and served HELLO-FROM-HOLO-NGINX over real TCP. insns={}", cpu.insns());
}

/// The server-image run config (shared by the G4 resume test and the browser-fixture generator): a real
/// nc server serving an HTTP body, with a self-testing wget client emitting `HOLO-SERVED-N` per fetch.
fn server_run_config() -> RunConfig {
    let script = r#"/bin/busybox ip link set lo up 2>/dev/null
printf 'HTTP/1.0 200 OK\r\nContent-Length: 21\r\n\r\nHELLO-FROM-HOLO-NGINX' > /resp
( while true; do /bin/busybox nc -l -p 8080 < /resp >/dev/null 2>&1; done ) &
/bin/busybox sleep 1
i=0
while true; do
  i=$((i+1))
  /bin/busybox wget -qO- http://127.0.0.1:8080/ >/dev/null 2>&1 && echo HOLO-SERVED-$i
  /bin/busybox sleep 1
done
"#;
    RunConfig {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()],
        workdir: "/".into(),
        uid: 0,
        gid: 0,
        net_up: false,
    }
}

fn server_rootfs() -> Vec<u8> {
    let layer = std::fs::read(art("vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz")).unwrap();
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).expect("compile image-init.c first");
    let init = image_init(&template, &server_run_config()).expect("patch image-init (server)");
    assemble_ext4_bootable(
        &[Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer }],
        &init,
        256 * 1024 * 1024,
    )
    .expect("assemble server rootfs")
}

/// Generate the warm SERVER-IMAGE κ-blob fixture for the CC-64 instant-paint browser path (G5): boot the
/// server until it is serving, snapshot the RUNNING machine → web/fixtures/x64-server-image.kblob. The
/// browser resumes this (paints the boot-log header instantly, streams the machine) and the server keeps
/// serving in the tab. Run: `cargo test -p holospaces --release --features net --test cc65_any_image_to_holo
/// generate_server_image_fixture -- --ignored --nocapture`.
#[test]
#[ignore = "boots a server image and writes a warm κ-blob fixture (slow, one-time)"]
fn generate_server_image_fixture() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, server_rootfs(), CMDLINE);
    let mut ready = false;
    for _ in 0..240 {
        cpu.run(20_000_000);
        if max_served(&String::from_utf8_lossy(cpu.console())) >= 2 {
            ready = true;
            break;
        }
    }
    assert!(ready, "server image never came up to serving before snapshot");
    let blob = cpu.snapshot_kappa_blob();
    let out = art("crates/holospaces-web/web/fixtures/x64-server-image.kblob");
    std::fs::write(&out, &blob).expect("write server-image kblob");
    eprintln!(
        "WROTE {} ({} MiB) — served={} at snapshot",
        out.display(),
        blob.len() / (1024 * 1024),
        max_served(&String::from_utf8_lossy(cpu.console()))
    );
}

/// Highest N in any `HOLO-SERVED-N` console line (0 if none) — each line is one successful in-guest fetch.
fn max_served(con: &str) -> u32 {
    con.lines()
        .filter_map(|l| l.trim().strip_prefix("HOLO-SERVED-"))
        .filter_map(|s| s.parse::<u32>().ok())
        .max()
        .unwrap_or(0)
}

/// CC-65 G4 — warm-snapshot a RUNNING server image and resume it: the low-latency payoff. The image's
/// server serves on a loop (a real nc server + a self-testing wget client emitting HOLO-SERVED-N per
/// successful fetch). We boot it until it's serving, `snapshot_kappa_blob()` the running machine, restore
/// it into a FRESH Cpu, and prove it KEEPS serving (a strictly higher HOLO-SERVED-N appears after resume)
/// — i.e. a live server image resumes warm and is already serving, no re-boot. Fixed-point like CC-59,
/// but on a running networked app.
#[test]
fn image_b_warm_snapshot_resume_keeps_serving() {
    let kernel = gunzip(art("vv/artifacts/cc45/linux/vmlinux.gz"));
    let layer = std::fs::read(art("vv/artifacts/cc45/alpine/alpine-minirootfs.tar.gz")).unwrap();
    let template = std::fs::read(art("vv/artifacts/cc65/image-init")).expect("compile image-init.c first");

    let script = r#"/bin/busybox ip link set lo up 2>/dev/null
printf 'HTTP/1.0 200 OK\r\nContent-Length: 21\r\n\r\nHELLO-FROM-HOLO-NGINX' > /resp
( while true; do /bin/busybox nc -l -p 8080 < /resp >/dev/null 2>&1; done ) &
/bin/busybox sleep 1
i=0
while true; do
  i=$((i+1))
  /bin/busybox wget -qO- http://127.0.0.1:8080/ >/dev/null 2>&1 && echo HOLO-SERVED-$i
  /bin/busybox sleep 1
done
"#;
    let cfg = RunConfig {
        argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
        env: vec!["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into()],
        workdir: "/".into(),
        uid: 0,
        gid: 0,
        net_up: false,
    };
    let init = image_init(&template, &cfg).expect("patch image-init (server loop)");
    let rootfs = assemble_ext4_bootable(
        &[Layer { media_type: "application/vnd.oci.image.layer.v1.tar+gzip", blob: &layer }],
        &init,
        256 * 1024 * 1024,
    )
    .expect("assemble bootable ext4 (server loop)");

    // Boot until the server has served at least twice (definitely up + looping).
    let mut cpu = Cpu::boot_linux_disk(512 * 1024 * 1024, &kernel, rootfs, CMDLINE);
    for _ in 0..240 {
        cpu.run(20_000_000);
        if max_served(&String::from_utf8_lossy(cpu.console())) >= 2 {
            break;
        }
    }
    let before = max_served(&String::from_utf8_lossy(cpu.console()));
    assert!(before >= 2, "server image never came up + served (got HOLO-SERVED-{before})");

    // Warm-snapshot the RUNNING server, then resume it into a fresh machine.
    let blob = cpu.snapshot_kappa_blob();
    let mut resumed = Cpu::new(0x1000);
    assert!(resumed.restore_kappa_blob(&blob), "restore the running-server .holo");
    assert_eq!(
        max_served(&String::from_utf8_lossy(resumed.console())),
        before,
        "resumed console is the snapshot's (fixed point)"
    );

    // Prove it KEEPS serving after the warm resume — a strictly higher HOLO-SERVED-N.
    let mut after = before;
    for _ in 0..240 {
        resumed.run(20_000_000);
        after = max_served(&String::from_utf8_lossy(resumed.console()));
        if after > before {
            break;
        }
    }
    assert!(
        after > before,
        "resumed server stopped serving (HOLO-SERVED stuck at {before}). tail:\n  {}",
        String::from_utf8_lossy(resumed.console()).lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n  ")
    );
    eprintln!(
        "CC-65 G4: warm-snapshot a RUNNING server image → resume → it KEEPS serving (HOLO-SERVED {before} → {after}). kblob={} MiB",
        blob.len() / (1024 * 1024)
    );
}
