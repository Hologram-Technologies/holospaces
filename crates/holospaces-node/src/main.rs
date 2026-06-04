//! The **holospaces node** binary — the peer a browser tab routes through. It
//! runs three roles a flashed device you own provides, none of which a tab can:
//!
//! - **egress** (`CC-16`/`CC-39`): forward the guest's arbitrary TCP to the real
//!   internet over the browser's egress WebSocket. Listen: `HOLOSPACES_NODE_ADDR`
//!   (default `0.0.0.0:9000`). A Chromebook points its holospace's egress at
//!   `ws://<node-ip>:9000`.
//! - **storage-sync** (`CC-38`): serve a persistent, file-backed content store
//!   over HTTP-CAS (`GET /cas/{κ}`). Store path: `HOLOSPACES_NODE_STORE` (default
//!   `./holospaces-node-store`); listen: `HOLOSPACES_NODE_CAS_ADDR` (default
//!   `0.0.0.0:9001`).
//! - **OTA** (Law L5): on startup, if `HOLOSPACES_NODE_OTA_URL` +
//!   `HOLOSPACES_NODE_OTA_KAPPA` are set, fetch the κ-addressed update from the
//!   Pages site, verify it re-derives, and stage it to `HOLOSPACES_NODE_OTA_STAGE`
//!   (default `./holospaces-node.staged`) for the next restart.

use std::net::TcpListener;
use std::path::PathBuf;

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn main() {
    // OTA: adopt a verified update before serving, if one is advertised.
    run_ota_if_configured();

    // Storage-sync: serve a persistent content store over HTTP-CAS.
    let store_path = env_or("HOLOSPACES_NODE_STORE", "./holospaces-node-store");
    let cas_addr = env_or("HOLOSPACES_NODE_CAS_ADDR", "0.0.0.0:9001");
    match holospaces_node::storage::open_store(&store_path) {
        Ok(store) => match holospaces_node::storage::serve(store, &cas_addr) {
            Ok(server) => {
                eprintln!(
                    "holospaces-node: storage-sync serving http://{}/cas/  (CC-38)",
                    server.addr()
                );
                // The CasServer runs on its own thread; keep it alive for the process.
                std::mem::forget(server);
            }
            Err(e) => {
                eprintln!("holospaces-node: storage-sync disabled (cannot serve {cas_addr}): {e}")
            }
        },
        Err(e) => {
            eprintln!("holospaces-node: storage-sync disabled (cannot open {store_path}): {e:?}")
        }
    }

    // Egress: forward guest TCP over the browser's egress WebSocket.
    let addr = env_or("HOLOSPACES_NODE_ADDR", "0.0.0.0:9000");
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("holospaces-node: cannot bind egress {addr}: {e}");
        std::process::exit(1);
    });
    eprintln!("holospaces-node: egress exit listening on ws://{addr}  (CC-16)");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                std::thread::spawn(move || holospaces_node::serve_connection(stream));
            }
            Err(e) => eprintln!("holospaces-node: accept failed: {e}"),
        }
    }
}

/// Fetch + verify + stage an OTA update if the site advertises one.
fn run_ota_if_configured() {
    let (Ok(url), Ok(kappa_str)) = (
        std::env::var("HOLOSPACES_NODE_OTA_URL"),
        std::env::var("HOLOSPACES_NODE_OTA_KAPPA"),
    ) else {
        return;
    };
    let Ok(kappa) = hologram_substrate_core::KappaLabel71::from_bytes(kappa_str.as_bytes()) else {
        eprintln!("holospaces-node: OTA skipped — HOLOSPACES_NODE_OTA_KAPPA is not a κ-label");
        return;
    };
    let stage = PathBuf::from(env_or(
        "HOLOSPACES_NODE_OTA_STAGE",
        "./holospaces-node.staged",
    ));
    match holospaces_node::ota::stage_update(&url, &kappa, &stage) {
        Ok(()) => eprintln!(
            "holospaces-node: OTA staged a verified update to {} (adopt on restart)",
            stage.display()
        ),
        Err(e) => eprintln!("holospaces-node: OTA not applied: {e}"),
    }
}
