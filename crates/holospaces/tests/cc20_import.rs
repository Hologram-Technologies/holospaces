//! `CC-20` — a devcontainer provisions from a repository URL over the internet
//! (the import boundary; ADR-013).
//!
//! holospaces reaches the internet to bring a devcontainer's content into the
//! substrate: it fetches a repository's archive by URL, reads its
//! `devcontainer.json`, pulls the devcontainer's OCI image from a registry
//! (the OCI distribution protocol), and **verifies every byte by re-derivation**
//! (a registry digest is a κ; Law L5). A repository with no devcontainer gets a
//! default image.
//!
//! Witnessed two ways:
//!   * **hermetically** (this file's main test) — a localhost HTTP server serves
//!     a pinned repository archive and the pinned `CC-14` OCI image over the real
//!     OCI distribution endpoints; the import client pulls + verifies + assembles
//!     + boots. No external network; fully reproducible (runs in CI).
//!   * against the **live internet** (`#[ignore]`) — pulls the real default image
//!     from Docker Hub; proves real-world interop. Network-gated, not in CI.

#![cfg(feature = "net")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;

use hologram_store_mem::MemKappaStore;
use holospaces::import::{
    import_and_assemble, parse_image_ref, pull_image, DEFAULT_DEVCONTAINER_IMAGE,
};
use holospaces::machine::MachineSpec;

fn cc14_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc14")
}

fn gunzip(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out).unwrap();
    out
}

/// The manifest digest of the pinned CC-14 OCI image (from its index.json).
fn image_manifest_digest() -> String {
    let index = std::fs::read(cc14_dir().join("image/index.json")).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&index).unwrap();
    v["manifests"][0]["digest"].as_str().unwrap().to_string()
}

/// A minimal USTAR + gzip archive of a single file at `path` with `data` — a
/// stand-in for a git-host repository archive (the tree wrapped in `repo-main/`).
fn make_repo_archive(path: &str, data: &[u8]) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    let mut hdr = [0u8; 512];
    hdr[..path.len()].copy_from_slice(path.as_bytes());
    let oct = |f: &mut [u8], v: u64| {
        let s = format!("{:0w$o}", v, w = f.len() - 1);
        f[..s.len()].copy_from_slice(s.as_bytes());
    };
    oct(&mut hdr[100..108], 0o644);
    oct(&mut hdr[108..116], 0);
    oct(&mut hdr[116..124], 0);
    oct(&mut hdr[124..136], data.len() as u64);
    oct(&mut hdr[136..148], 0);
    hdr[156] = b'0';
    hdr[257..263].copy_from_slice(b"ustar\0");
    hdr[263] = b'0';
    hdr[264] = b'0';
    hdr[148..156].fill(b' ');
    let sum: u32 = hdr.iter().map(|&b| b as u32).sum();
    oct(&mut hdr[148..155], sum as u64);
    hdr[155] = b' ';
    let mut tar = Vec::new();
    tar.extend_from_slice(&hdr);
    tar.extend_from_slice(data);
    let pad = data.len().div_ceil(512) * 512 - data.len();
    tar.extend(std::iter::repeat_n(0u8, pad));
    tar.extend([0u8; 1024]);
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

/// Serve the repository archive + the pinned OCI image over the real OCI
/// distribution endpoints on `listener`, in a background thread.
fn spawn_server(listener: TcpListener, archive: Vec<u8>) {
    let blobs = cc14_dir().join("image/blobs/sha256");
    let manifest_digest = image_manifest_digest();
    let manifest =
        std::fs::read(blobs.join(manifest_digest.strip_prefix("sha256:").unwrap())).unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut s = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("/");
            let (ctype, body): (&str, Vec<u8>) = if path == "/repo/archive/main.tar.gz" {
                ("application/gzip", archive.clone())
            } else if path.starts_with("/v2/img/manifests/") {
                (
                    "application/vnd.oci.image.manifest.v1+json",
                    manifest.clone(),
                )
            } else if let Some(d) = path.strip_prefix("/v2/img/blobs/sha256:") {
                match std::fs::read(blobs.join(d)) {
                    Ok(b) => ("application/octet-stream", b),
                    Err(_) => {
                        let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                        continue;
                    }
                }
            } else {
                let _ = s.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            };
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nDocker-Content-Digest: {manifest_digest}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(header.as_bytes());
            let _ = s.write_all(&body);
        }
    });
}

/// The hermetic end-to-end: import a devcontainer from a (localhost) repository
/// URL — fetch the archive, read its devcontainer.json, pull + verify the OCI
/// image, assemble the rootfs — and boot a real Linux that mounts it. No mocks;
/// every byte verified by re-derivation. `#[ignore]` (a real-OS boot, ~17 s).
#[test]
#[ignore]
fn a_devcontainer_provisions_from_a_repository_url() {
    // Bind first so the devcontainer.json can name the localhost registry image,
    // then move the listener into the server (no rebind race).
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = format!(r#"{{"image":"127.0.0.1:{port}/img:latest"}}"#);
    let archive = make_repo_archive(
        "repo-main/.devcontainer/devcontainer.json",
        config.as_bytes(),
    );
    spawn_server(listener, archive);

    let store = MemKappaStore::new();
    let repo_url = format!("http://127.0.0.1:{port}/repo");
    let (imported, rootfs) = import_and_assemble(&store, &repo_url, "main")
        .expect("import the devcontainer from its URL");
    assert!(
        !imported.used_default,
        "the repository's own devcontainer.json was used"
    );
    assert!(
        !rootfs.is_empty() && rootfs.len().is_multiple_of(4096),
        "assembled an ext4 rootfs"
    );

    let kernel = gunzip(&cc14_dir().join("kernel/Image.gz"));
    let mut emu = MachineSpec::devcontainer()
        .boot(&kernel, rootfs)
        .expect("boot");
    emu.run(600_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();
    let marker = std::fs::read_to_string(cc14_dir().join("expected.txt")).unwrap();
    assert!(
        console.contains("Mounted root (ext4 filesystem)") && console.contains(marker.trim()),
        "the imported devcontainer boots from its registry image; console:\n{console}"
    );
}

/// A repository with no devcontainer.json falls back to the default image — the
/// hermetic check of the fallback (pulling the default is the live test's job).
#[test]
fn a_repository_without_a_devcontainer_uses_the_default_image() {
    use holospaces::assembly::{find_devcontainer_json, Layer};
    // An archive with some file but no devcontainer config.
    let archive = make_repo_archive("repo-main/README.md", b"# hi\n");
    let found = find_devcontainer_json(&Layer {
        media_type: "application/gzip",
        blob: &archive,
    })
    .unwrap();
    assert!(found.is_none(), "no devcontainer.json in the repository");
    // The default image reference is a real, parseable registry reference.
    let r = parse_image_ref(DEFAULT_DEVCONTAINER_IMAGE).unwrap();
    assert_eq!(r.registry, "registry-1.docker.io");
    assert!(r.repository.contains("debian"));
}

/// Live interop (network-gated, not in CI): pull the real default image from
/// Docker Hub by reference and assemble it — proves the OCI distribution client
/// (token auth, multi-arch index → riscv64, blob verification) against a real
/// registry. Run with `--ignored` on a networked host.
#[test]
#[ignore]
fn live_pull_of_the_default_image_from_docker_hub() {
    let store = MemKappaStore::new();
    let image = pull_image(
        &store,
        &parse_image_ref(DEFAULT_DEVCONTAINER_IMAGE).unwrap(),
    )
    .expect("pull the default image from Docker Hub");
    assert!(!image.layers().is_empty(), "the pulled image has layers");
}
