//! `CC-58` — the assembled `ext4` preserves file **mode bits** (the executable
//! bit on `/init` and on image binaries).
//!
//! A real image whose `/init` is a shell script (`#!/bin/busybox sh`) or whose
//! binaries need `+x` only boots if the assembled ext4 carries each entry's
//! permission bits: a kernel `exec` of a non-executable `/init` fails
//! `EACCES (-13)`. This witnesses that the mode survives the whole pipeline —
//!
//!   * tar USTAR mode (header offset 100, octal) → the overlay [`Node`]'s
//!     `Meta.mode` → the ext4 writer's inode `i_mode` (the `crate::oci` real-image
//!     path), and
//!   * the injected-file path ([`assemble_ext4_with_files`]) honouring its `u16`
//!     mode argument (how the Boot Orchestrator places `/init`, `CC-22`).
//!
//! The external authority (V&V oracle, not a runtime dependency) is the same as
//! `CC-14`: e2fsprogs reads the assembled image back independently of holospaces'
//! own writer —
//!
//! - **e2fsprogs** `debugfs` — `stat <path>` reports an inode's Mode; that is the
//!   external authority for "is this file 0755?". Asserting the writer's own bytes
//!   would be self-referential; debugfs decodes the on-disk inode independently.
//! - **e2fsprogs** `e2fsck -fn` — the structure must stay clean (`rc == 0`), so
//!   the mode is carried without corrupting the layout `CC-14` already accepts.
//!
//! Two further real-image fidelity properties (adopted from the canonical
//! upstream ext4 writer — see the cross-check reconciliation) are witnessed here
//! against the same oracle, since both decide whether a *real* registry image
//! assembles soundly:
//!
//!   * **ownership ≥ 65536** — rootless / user-namespaced images own files at
//!     high uids (e.g. `100000:100000`); the writer must emit the inode's
//!     `l_i_uid_high` / `l_i_gid_high` (osd2) or `debugfs` reports a truncated
//!     `uid & 0xffff` (silently wrong ownership);
//!   * **large multi-group disks** — a build-capable rootfs spans many 128 MiB
//!     block groups; the geometry must round a multi-group image to whole groups
//!     so the final group's metadata is never written past the image end (an
//!     out-of-bounds image for any large disk). `e2fsck -fn` is the authority.

use std::path::Path;
use std::process::Command;

use holospaces::assembly::{assemble_ext4, assemble_ext4_bootable, assemble_ext4_with_files, Layer};

fn have_tool(name: &str) -> bool {
    Command::new(name)
        .arg("-V")
        .output()
        .map(|o| o.status.success() || !o.stderr.is_empty())
        .unwrap_or(false)
}

/// Build a minimal uncompressed USTAR archive carrying each entry's explicit
/// octal mode (header offset 100..108) — a stand-in for a real OCI layer that
/// ships an executable binary, exercising the tar→`Node` arm of the pipeline.
fn make_tar(entries: &[(&[u8], u8, u32, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, typeflag, mode, data) in entries {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name);
        let wr = |field: &mut [u8], v: u64| {
            let s = format!("{:0width$o}", v, width = field.len() - 1);
            field[..s.len()].copy_from_slice(s.as_bytes());
        };
        wr(&mut h[100..108], *mode as u64);
        wr(&mut h[124..136], data.len() as u64);
        wr(&mut h[136..148], 0);
        h[156] = *typeflag;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263] = b'0';
        h[264] = b'0';
        for c in h.iter_mut().skip(148).take(8) {
            *c = b' ';
        }
        let sum: u32 = h.iter().map(|&b| b as u32).sum();
        wr(&mut h[148..155], sum as u64);
        h[155] = b' ';
        out.extend_from_slice(&h);
        out.extend_from_slice(data);
        let pad = data.len().div_ceil(512) * 512 - data.len();
        out.resize(out.len() + pad, 0);
    }
    out.extend([0u8; 1024]); // two zero end blocks
    out
}

/// Ask the external oracle for an inode's mode: `debugfs -R "stat <path>"` reports
/// a line `Inode: N   Type: …    Mode:  0755   Flags: …`. Returns the octal `0755`
/// string debugfs decoded from the on-disk inode (None if the line is absent).
fn oracle_mode(img: &Path, path: &str) -> Option<String> {
    let out = Command::new("debugfs")
        .arg("-R")
        .arg(format!("stat {path}"))
        .arg(img)
        .output()
        .expect("run debugfs");
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        if let Some(idx) = line.find("Mode:") {
            // "… Mode:  0755   Flags: …" → take the token after "Mode:".
            return line[idx + "Mode:".len()..]
                .split_whitespace()
                .next()
                .map(str::to_string);
        }
    }
    None
}

/// The assembled ext4 carries each entry's permission bits: an executable
/// `/init` reads back Mode 0755, an executable image binary 0755, and a plain
/// file 0644 — proven by the e2fsprogs oracle, with `e2fsck -fn` clean. This is
/// the fast (no-boot) behavioural proof that `exec /init` will not fail `EACCES`.
#[test]
fn assembled_ext4_preserves_file_mode_bits() {
    if !have_tool("e2fsck") || !have_tool("debugfs") {
        eprintln!("SKIP: e2fsprogs (e2fsck/debugfs) not available");
        return;
    }

    // Arm 1 (real-image path): a tar layer shipping an executable binary (0755),
    // a plain config file (0644), and the directory that holds them.
    let tar = make_tar(&[
        (b"bin/", b'5', 0o755, b""),
        (b"bin/busybox", b'0', 0o755, b"\x7fELF stand-in binary"),
        (b"etc/hostname", b'0', 0o644, b"holospaces\n"),
    ]);
    let layers = [Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar",
        blob: &tar,
    }];

    // Arm 2 (injection path): `/init` injected as the shell-script lifecycle
    // runner at mode 0755 (the `import_image` `("init", 0o755, …)` hook), plus a
    // non-executable injected file at 0644.
    let init = b"#!/bin/busybox sh\nexec /bin/busybox sh\n";
    let files: &[(&str, u16, &[u8])] = &[
        ("init", 0o755, init),
        ("etc/profile", 0o644, b"export PATH=/bin\n"),
    ];

    let image = assemble_ext4_with_files(&layers, files).expect("assemble the rootfs ext4");
    assert!(
        image.len().is_multiple_of(4096),
        "ext4 image is a whole number of blocks"
    );

    let dir = std::env::temp_dir();
    let path = dir.join("cc58-perms.ext4");
    std::fs::write(&path, &image).unwrap();

    // ── External oracle 1: e2fsck must still find the structure clean ──
    let fsck = Command::new("e2fsck")
        .args(["-fn"])
        .arg(&path)
        .output()
        .expect("run e2fsck");
    assert!(
        fsck.status.success(),
        "e2fsck must find the assembled ext4 clean (rc {:?}):\n{}\n{}",
        fsck.status.code(),
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr),
    );

    // ── External oracle 2: debugfs reports the preserved mode for each entry ──
    // (the on-disk inode decoded independently of holospaces' own writer).
    let cases = [
        ("/init", "0755"),          // injected exec /init — the EACCES guard
        ("/bin/busybox", "0755"),   // tar-sourced exec binary
        ("/etc/hostname", "0644"),  // tar-sourced plain file
        ("/etc/profile", "0644"),   // injected plain file
    ];
    for (p, want) in cases {
        let got = oracle_mode(&path, p)
            .unwrap_or_else(|| panic!("debugfs reported no Mode for {p}"));
        assert_eq!(
            got, want,
            "debugfs must report Mode {want} for {p} (got {got}) — the mode bit \
             must survive assembly so exec does not fail EACCES"
        );
    }

    std::fs::remove_file(&path).ok();
}

/// `debugfs -R "stat <path>"` reports `User: <n>   Group: <m>   …`. Return the
/// decimal value debugfs decoded for `field` ("User" or "Group") from the on-disk
/// inode — the external authority for the file's full uid/gid.
fn oracle_owner(img: &Path, path: &str, field: &str) -> Option<u32> {
    let out = Command::new("debugfs")
        .arg("-R")
        .arg(format!("stat {path}"))
        .arg(img)
        .output()
        .expect("run debugfs");
    let text = String::from_utf8_lossy(&out.stdout);
    let key = format!("{field}:");
    for line in text.lines() {
        if let Some(idx) = line.find(&key) {
            return line[idx + key.len()..]
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<u32>().ok());
        }
    }
    None
}

/// The assembled ext4 preserves **ownership ≥ 65536**: a tar entry owned by uid/gid
/// `100000` (a rootless / user-namespaced image's files) reads back as `100000`,
/// not the `34464` (`100000 & 0xffff`) a writer that drops `l_i_uid_high` /
/// `l_i_gid_high` would emit. Witnessed against the debugfs oracle, `e2fsck` clean.
#[test]
fn assembled_ext4_preserves_high_uid_gid_ownership() {
    if !have_tool("e2fsck") || !have_tool("debugfs") {
        eprintln!("SKIP: e2fsprogs (e2fsck/debugfs) not available");
        return;
    }
    // A USTAR entry owned by uid/gid 100000 (offsets 108/116). `make_tar` writes
    // only the mode, so build the header here with explicit owner fields.
    let mut h = [0u8; 512];
    h[..3].copy_from_slice(b"app");
    let data = b"owned by a rootless uid\n";
    let wr = |field: &mut [u8], v: u64| {
        let s = format!("{:0width$o}", v, width = field.len() - 1);
        field[..s.len()].copy_from_slice(s.as_bytes());
    };
    wr(&mut h[100..108], 0o644);
    wr(&mut h[108..116], 100_000); // uid
    wr(&mut h[116..124], 100_000); // gid
    wr(&mut h[124..136], data.len() as u64);
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263] = b'0';
    h[264] = b'0';
    for c in h.iter_mut().skip(148).take(8) {
        *c = b' ';
    }
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    wr(&mut h[148..155], sum as u64);
    h[155] = b' ';
    let mut tar = h.to_vec();
    tar.extend_from_slice(data);
    tar.resize(tar.len() + (512 - data.len() % 512) % 512, 0);
    tar.extend([0u8; 1024]);

    let image = assemble_ext4(&[Layer {
        media_type: "application/vnd.oci.image.layer.v1.tar",
        blob: &tar,
    }])
    .expect("assemble the rootfs ext4");
    let path = std::env::temp_dir().join("cc58-owner.ext4");
    std::fs::write(&path, &image).unwrap();

    let fsck = Command::new("e2fsck").args(["-fn"]).arg(&path).output().expect("run e2fsck");
    assert!(fsck.status.success(), "e2fsck clean on the high-uid image");

    assert_eq!(
        oracle_owner(&path, "/app", "User"),
        Some(100_000),
        "debugfs must report the full uid 100000, not a truncated uid & 0xffff"
    );
    assert_eq!(
        oracle_owner(&path, "/app", "Group"),
        Some(100_000),
        "debugfs must report the full gid 100000 (l_i_gid_high preserved)"
    );
    std::fs::remove_file(&path).ok();
}

/// A **large, multi-block-group** ext4 (a build-capable rootfs) is sound: sizing a
/// 130 MiB disk crosses the 128 MiB single-group boundary, so the writer must round
/// to whole block groups and place every group's metadata inside the image. `e2fsck
/// -fn` (the authority) must find ≥ 2 groups and zero errors — without the rounding
/// the final short group's inode table lands past the image end (a broken disk).
#[test]
fn assembled_ext4_large_multigroup_disk_is_e2fsck_clean() {
    if !have_tool("e2fsck") {
        eprintln!("SKIP: e2fsprogs (e2fsck) not available");
        return;
    }
    // 130 MiB > one 128 MiB block group → a multi-group image with a short trailing
    // group before rounding (the case the geometry fix must make sound).
    let image = assemble_ext4_bootable(&[], b"#!/bin/sh\n", 130 * 1024 * 1024)
        .expect("assemble a large bootable ext4");
    let blocks = image.len() / 4096;
    assert!(
        blocks > 32_768,
        "the image spans more than one 32768-block group ({blocks} blocks)"
    );
    let path = std::env::temp_dir().join("cc58-multigroup.ext4");
    std::fs::write(&path, &image).unwrap();
    let fsck = Command::new("e2fsck").args(["-fn"]).arg(&path).output().expect("run e2fsck");
    assert!(
        fsck.status.success(),
        "e2fsck must find the large multi-group image clean (rc {:?}):\n{}\n{}",
        fsck.status.code(),
        String::from_utf8_lossy(&fsck.stdout),
        String::from_utf8_lossy(&fsck.stderr),
    );
    std::fs::remove_file(&path).ok();
}
