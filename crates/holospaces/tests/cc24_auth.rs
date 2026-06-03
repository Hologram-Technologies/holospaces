//! `CC-24` — the devcontainer authenticates with GitHub (and other services)
//! over the holospaces network.
//!
//! A Codespace/Gitpod lets you sign in to GitHub and other services from the
//! environment. holospaces does not intermediate that auth — it provides the
//! *network* (`CC-16`), and the devcontainer's tools (here a freestanding
//! stand-in for `gh auth login`) authenticate over it using the service's own
//! published OAuth flow: the **OAuth 2.0 Device Authorization Grant (RFC 8628)**,
//! which needs no backend secret and so works from a browser/Pages deployment.
//! **The token lives in the devcontainer, never in holospaces** (no held secrets;
//! holospaces is content-blind, Laws L1/L3; ADR-017).
//!
//! The external authority is **RFC 8628**; the differential oracle is a hermetic
//! server implementing the device-code + token endpoints exactly as GitHub does
//! (`POST /login/device/code` → `device_code`/`user_code`; `POST
//! /login/oauth/access_token` → `authorization_pending` until authorized, then an
//! `access_token`). The guest performs the whole flow over its `virtio-net`
//! interface (the `CC-16` userspace NAT + native egress, port-forwarded to the
//! hermetic server as `gh` would reach `github.com`).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::assembly::{assemble_ext4, Layer};
use holospaces::emulator::net::StdEgress;
use holospaces::machine::MachineSpec;
use holospaces::oci::{ingest_image, IngestedImage, OciError};

fn cc24_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc24")
}
fn cc16_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc16")
}

fn blob_bytes(digest: &str) -> Option<Vec<u8>> {
    let hex = digest.strip_prefix("sha256:")?;
    std::fs::read(cc24_dir().join("image/blobs/sha256").join(hex)).ok()
}
fn ingest(store: &MemKappaStore) -> Result<IngestedImage, OciError> {
    let layout = std::fs::read(cc24_dir().join("image/oci-layout")).unwrap();
    let index = std::fs::read(cc24_dir().join("image/index.json")).unwrap();
    ingest_image(
        store,
        &layout,
        &index,
        holospaces::Arch::Riscv64,
        blob_bytes,
    )
}
fn gunzip(path: &Path) -> Vec<u8> {
    let raw = std::fs::read(path).unwrap();
    let mut d = flate2::read::GzDecoder::new(&raw[..]);
    let mut out = Vec::new();
    d.read_to_end(&mut out).unwrap();
    out
}

/// A hermetic RFC 8628 server (GitHub's device-flow endpoints): hands out a
/// device code, holds `authorization_pending` for the first poll (as if the user
/// has not yet entered the code), then issues an access token.
fn spawn_device_flow_server() -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let polls = Arc::new(AtomicUsize::new(0));
    let handle = std::thread::spawn(move || {
        // Serve a bounded number of connections (device-code + a few polls).
        for _ in 0..8 {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            let polls = polls.clone();
            let mut buf = [0u8; 2048];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let body = if req.contains("/login/device/code") {
                // RFC 8628 §3.2 device authorization response.
                String::from(
                    "{\"device_code\":\"DC-holospaces-0xCAFE\",\"user_code\":\"WDJB-MJHT\",\"verification_uri\":\"https://github.com/login/device\",\"expires_in\":900,\"interval\":1}",
                )
            } else if req.contains("/login/oauth/access_token") {
                // RFC 8628 §3.4/§3.5: pending until authorized, then the token.
                if polls.fetch_add(1, Ordering::SeqCst) == 0 {
                    String::from("{\"error\":\"authorization_pending\"}")
                } else {
                    String::from("{\"access_token\":\"gho_holospaceTESTtoken123\",\"token_type\":\"bearer\",\"scope\":\"repo\"}")
                }
            } else {
                String::from("{}")
            };
            let resp = format!(
                "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    (port, handle)
}

/// The devcontainer performs the GitHub device flow over the holospaces network
/// and obtains an access token. Heavy (a real-OS boot), so `#[ignore]`d.
#[test]
#[ignore]
fn the_devcontainer_authenticates_with_github_over_the_network() {
    let (port, server) = spawn_device_flow_server();

    // Assemble the auth-init rootfs from its OCI image.
    let store = MemKappaStore::new();
    let img = ingest(&store).expect("ingest the CC-24 image");
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
    let rootfs = assemble_ext4(&layers).expect("assemble rootfs");

    // The guest dials github.com (10.0.2.9:80); the native egress port-forwards
    // that to the hermetic RFC 8628 server — exactly as a slirp/Codespaces NAT
    // routes the devcontainer's outbound auth traffic.
    let egress = StdEgress::new().redirect([10, 0, 2, 9], 80, "127.0.0.1", port);
    let kernel = gunzip(&cc16_dir().join("kernel/Image.gz"));
    let mut emu = MachineSpec::devcontainer_net()
        .boot_net(&kernel, rootfs, Box::new(egress))
        .expect("boot with networking");
    emu.run(900_000_000);
    let console = String::from_utf8_lossy(emu.console()).into_owned();
    // Don't join the server: it loops on `accept()` and the guest opens only a
    // few connections, so joining would block. The assertions are on the console;
    // the detached server thread is reaped at process exit.
    drop(server);

    assert!(
        console.contains("DEVICE-CODE-OK"),
        "the devcontainer completed the device-authorization request (RFC 8628 §3.2); console:\n{console}"
    );
    assert!(
        console.contains("POLL-PENDING"),
        "the devcontainer polled and saw authorization_pending (RFC 8628 §3.5); console:\n{console}"
    );
    assert!(
        console.contains("AUTH-OK:gho_holospaceTESTtoken123"),
        "the devcontainer obtained the GitHub access token over the holospaces network — the token lives in the devcontainer (CC-24, ADR-017); console:\n{console}"
    );
}
