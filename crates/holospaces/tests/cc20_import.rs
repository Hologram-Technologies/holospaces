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
use hologram_substrate_core::{Bytes, KappaStore};
use holospaces::assembly::{overlay_layers, Layer, Node};
use holospaces::import::{
    import_and_assemble, parse_image_ref, pull_image, DEFAULT_DEVCONTAINER_IMAGE,
};
use holospaces::machine::MachineSpec;
use holospaces::Arch;

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

/// The manifest digest of an OCI image fixture (from its `index.json`).
fn manifest_digest_of(image_dir: &Path) -> String {
    let index = std::fs::read(image_dir.join("index.json")).unwrap();
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
    spawn_server_for(listener, archive, cc14_dir().join("image"));
}

/// Serve a repository archive + an arbitrary OCI image fixture over the real OCI
/// distribution endpoints — so the import client pulls whatever image the repo's
/// devcontainer.json declares, from any fixture (the basis of the "arbitrary"
/// proof: different repos, different real images, the same import path).
fn spawn_server_for(listener: TcpListener, archive: Vec<u8>, image_dir: PathBuf) {
    let blobs = image_dir.join("blobs/sha256");
    let manifest_digest = manifest_digest_of(&image_dir);
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
    let (imported, rootfs) = import_and_assemble(&store, &repo_url, "main", Arch::Riscv64)
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

/// **holospaces boots *arbitrary* real devcontainers** — not one demo image. For
/// each of two *distinct* real OCI images (the `CC-14` base and the `CC-16`
/// networked base, different fixtures), a repository declares it in its own
/// devcontainer.json; the import client pulls + verifies + assembles *that*
/// image, and a real Linux boots on it (mounts the assembled ext4 root). The same
/// repo-URL → image → boot path, two different real images — so the launched
/// holospace is the repository's actual devcontainer, whatever it declares, not a
/// fixed demo. `#[ignore]` (two real-OS boots; release / the CC-20 vv suite).
#[test]
#[ignore]
fn holospaces_boots_arbitrary_real_devcontainers() {
    // (fixture image dir, its kernel, an optional content marker proving it is
    // *that* image's userland, not a stand-in).
    let cases: &[(PathBuf, PathBuf, Option<String>)] = &[
        (
            cc14_dir().join("image"),
            cc14_dir().join("kernel/Image.gz"),
            Some(std::fs::read_to_string(cc14_dir().join("expected.txt")).unwrap()),
        ),
        (
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc16/image"),
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc16/kernel/Image.gz"),
            None,
        ),
    ];

    for (image_dir, kernel_path, marker) in cases {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // The repository declares THIS image — the path follows the repo, not a
        // hardcode.
        let config = format!(r#"{{"image":"127.0.0.1:{port}/img:latest"}}"#);
        let archive = make_repo_archive(
            "repo-main/.devcontainer/devcontainer.json",
            config.as_bytes(),
        );
        spawn_server_for(listener, archive, image_dir.clone());

        let store = MemKappaStore::new();
        let repo_url = format!("http://127.0.0.1:{port}/repo");
        let (imported, rootfs) = import_and_assemble(&store, &repo_url, "main", Arch::Riscv64)
            .unwrap_or_else(|e| panic!("import {} failed: {e:?}", image_dir.display()));
        assert!(
            !imported.used_default,
            "the repository's own declared image was used ({})",
            image_dir.display()
        );

        let kernel = gunzip(kernel_path);
        let mut emu = MachineSpec::devcontainer()
            .boot(&kernel, rootfs)
            .unwrap_or_else(|e| panic!("boot {} failed: {e:?}", image_dir.display()));
        emu.run(600_000_000);
        let console = String::from_utf8_lossy(emu.console()).into_owned();
        assert!(
            console.contains("Mounted root (ext4 filesystem)"),
            "a real Linux booted on the imported image {}; console:\n{console}",
            image_dir.display()
        );
        if let Some(m) = marker {
            assert!(
                console.contains(m.trim()),
                "the booted system is that image's real userland ({}); console:\n{console}",
                image_dir.display()
            );
        }
    }
}

/// A real-OS devcontainer's bootable content travels the **substrate's
/// content-addressed transport**: an importer peer assembles the rootfs and
/// serves its store as an *untrusted* HTTP-CAS gateway (`hologram-net-http`,
/// the substrate's `/cas/{κ}` protocol); a second peer holding **no local
/// content** fetches the kernel + rootfs by κ through `get_with_fetch`, which
/// **verifies each on receipt** (Law L5 — a tampered byte is refused), caches
/// them, and boots a real Linux on the fetched rootfs.
///
/// This is the exact path the **browser peer** takes to boot a devcontainer the
/// page did not assemble locally: its `fetch()` is the same `/cas/{κ}` client
/// and the verify-on-receipt happens in wasm (`verify_kappa`), so the boot
/// content is delivered trustlessly from a generic hologram gateway — no
/// bespoke server, no trust in the gateway. Witnessed hermetically here (a real
/// import + a real boot; `#[ignore]`, ~17 s).
#[test]
#[ignore]
fn a_devcontainer_boots_on_a_peer_that_fetched_it_from_a_substrate_cas_gateway() {
    use hologram_net_http::live::{serve_addr, HttpKappaSync};
    use hologram_substrate_core::get_with_fetch;
    use std::sync::Arc;

    // Importer peer: import + assemble a devcontainer from a localhost repo +
    // registry, then publish the bootable artifacts (rootfs + kernel) as κ
    // content in its store.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = format!(r#"{{"image":"127.0.0.1:{port}/img:latest"}}"#);
    let archive = make_repo_archive(
        "repo-main/.devcontainer/devcontainer.json",
        config.as_bytes(),
    );
    spawn_server(listener, archive);

    let gateway: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());
    let repo_url = format!("http://127.0.0.1:{port}/repo");
    let (_imported, rootfs) =
        import_and_assemble(gateway.as_ref(), &repo_url, "main", Arch::Riscv64)
            .expect("import the devcontainer from its URL");
    let kernel = gunzip(&cc14_dir().join("kernel/Image.gz"));
    let rootfs_k = gateway.put("blake3", &rootfs).unwrap();
    let kernel_k = gateway.put("blake3", &kernel).unwrap();

    // Serve the importer's store as an UNTRUSTED content-addressed gateway.
    let server = serve_addr(gateway.clone(), "127.0.0.1:0", false).expect("serve HTTP-CAS");

    // Second peer: an empty store + a sync client pointed at the gateway. It
    // fetches the bootable artifacts by κ — verifying each on receipt.
    let local = MemKappaStore::new();
    let sync = HttpKappaSync::new(vec![server.addr().to_string()]);
    let (got_rootfs, got_kernel) = pollster::block_on(async {
        let r = get_with_fetch(&local, &sync, &rootfs_k)
            .await
            .expect("fetch rootfs from the gateway")
            .expect("the gateway holds the rootfs");
        let k = get_with_fetch(&local, &sync, &kernel_k)
            .await
            .expect("fetch kernel from the gateway")
            .expect("the gateway holds the kernel");
        (r, k)
    });
    assert!(
        local.contains(&rootfs_k) && local.contains(&kernel_k),
        "the fetched content verified on receipt and is cached locally (Law L5)"
    );

    // Boot a real Linux on the gateway-fetched rootfs — no local import.
    let mut emu = MachineSpec::devcontainer()
        .boot(&got_kernel, got_rootfs.to_vec())
        .expect("boot");
    emu.run(600_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();
    let marker = std::fs::read_to_string(cc14_dir().join("expected.txt")).unwrap();
    assert!(
        console.contains("Mounted root (ext4 filesystem)") && console.contains(marker.trim()),
        "the devcontainer boots from gateway-fetched content; console:\n{console}"
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
    // The default image reference is a real, parseable registry reference — the
    // Codespaces-style *usable* default (`buildpack-deps`: curl/git/wget over a
    // real apt userland), not a bare base.
    let r = parse_image_ref(DEFAULT_DEVCONTAINER_IMAGE).unwrap();
    assert_eq!(r.registry, "registry-1.docker.io");
    assert_eq!(r.repository, "library/buildpack-deps");
}

/// Live interop (network-gated, not in CI): pull the real default image from
/// Docker Hub by reference and assemble it — proves the OCI distribution client
/// (token auth, multi-arch index → riscv64, blob verification) against a real
/// registry. Run with `--ignored` on a networked host.
///
/// It also witnesses that the default is a **usable** environment, not a bare
/// base: the assembled layers carry the developer utilities an operator expects
/// on entry (`curl`, `git`) — the Codespaces "open a repo with no config and it
/// just works" promise. Asserted for **both** emulator architectures, since the
/// default must boot on either (`riscv64`/`aarch64`, ADR-021).
#[test]
#[ignore]
fn live_pull_of_the_default_image_from_docker_hub() {
    for arch in [Arch::Riscv64, Arch::Aarch64] {
        let store = MemKappaStore::new();
        let image = pull_image(
            &store,
            &parse_image_ref(DEFAULT_DEVCONTAINER_IMAGE).unwrap(),
            arch,
        )
        .unwrap_or_else(|e| panic!("pull the default image from Docker Hub ({arch:?}): {e:?}"));
        assert!(!image.layers().is_empty(), "the pulled image has layers");

        // The default ships basic developer tooling — witnessed in the *actual
        // assembled filesystem* the OS boots: overlay the image's layers (lowest
        // first, honouring OCI whiteouts — the same path `assemble_ext4` takes)
        // and resolve the binaries through the resulting tree. Not a byte scan:
        // this is the real `/usr/bin/curl` an operator runs on entry.
        let media = image.layer_media_types();
        let blobs: Vec<Bytes> = image
            .layers()
            .iter()
            .map(|k| store.get(k).unwrap().expect("layer blob in store"))
            .collect();
        let layers: Vec<Layer> = blobs
            .iter()
            .zip(media)
            .map(|(blob, mt)| Layer {
                media_type: mt,
                blob,
            })
            .collect();
        let tree = overlay_layers(&layers).expect("overlay the default image's layers");

        assert!(
            tree_has_regular_file(&tree.root, "usr/bin/curl"),
            "the default image provides curl ({arch:?})"
        );
        assert!(
            tree_has_regular_file(&tree.root, "usr/bin/git"),
            "the default image provides git ({arch:?})"
        );
    }
}

/// Whether `path` (slash-separated, relative to root) resolves to a regular file
/// in the overlaid filesystem `root` — a true filesystem-tree walk (each
/// non-final component must be a directory), so it witnesses a real executable,
/// not a stray substring.
fn tree_has_regular_file(root: &Node, path: &str) -> bool {
    let mut node = root;
    let mut parts = path.split('/').filter(|p| !p.is_empty()).peekable();
    while let Some(name) = parts.next() {
        let Node::Dir { children, .. } = node else {
            return false;
        };
        let Some(child) = children.get(name) else {
            return false;
        };
        if parts.peek().is_none() {
            return matches!(child, Node::File { .. });
        }
        node = child;
    }
    false
}
