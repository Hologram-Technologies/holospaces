//! **Storage-sync — the node as a persistent content peer.**
//!
//! A browser tab's store is RAM (lost on reload); a flashed node has durable
//! flash/SD. The node opens a persistent, file-backed `KappaStore`
//! ([`hologram_store_native::NativeKappaStore`]) and serves it over the
//! substrate's **HTTP-CAS** protocol — `GET /cas/{κ}`, the *same* protocol the
//! browser peer fetches by (its `fetch()` → `Console::receive`, `CC-20`) and the
//! same a peer fetches over `get_with_fetch`. So the operator's content survives
//! reloads and devices, served trustlessly: a content peer **verifies every byte
//! by re-derivation on receipt** (Law L5), and the node `serve`s with
//! `forge = false`, so it can never fabricate content for a κ.

use std::path::Path;
use std::sync::Arc;

use hologram_net_http::live::{serve_addr, CasServer};
use hologram_store_native::NativeKappaStore;
use hologram_substrate_core::{KappaStore, StoreError};

/// Open the node's **persistent** content store at `path` (the device's
/// flash/SD). Content put here survives a restart — the durable half of
/// storage-sync.
pub fn open_store(path: impl AsRef<Path>) -> Result<Arc<dyn KappaStore>, StoreError> {
    Ok(Arc::new(NativeKappaStore::open(path)?))
}

/// Serve a content `store` over the substrate's HTTP-CAS protocol at `addr`
/// (`GET /cas/{κ}`). `forge` is **false**: the node serves only content that
/// re-derives to the requested κ — it cannot fabricate (the peer also verifies on
/// receipt, Law L5). Returns the running server; dropping it stops serving.
pub fn serve(store: Arc<dyn KappaStore>, addr: &str) -> std::io::Result<CasServer> {
    serve_addr(store, addr, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hologram_net_http::cas_path;
    use hologram_substrate_core::verify_kappa;
    use std::io::Read;

    /// A unique temp directory for a test store (no `Date`/random needed).
    fn temp_store_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "holospaces-node-{tag}-{}-{:p}",
            std::process::id(),
            &tag
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Content put into the node's store survives a restart (re-open) — the
    /// durable half of storage-sync.
    #[test]
    fn content_persists_across_a_restart() {
        let dir = temp_store_dir("persist");
        let content = b"the operator's content, persisted on the node";

        let kappa = {
            let store = open_store(&dir).unwrap();
            store.put("blake3", content).unwrap()
        }; // the store is dropped here — simulating the node restarting.

        let reopened = open_store(&dir).unwrap();
        assert_eq!(
            reopened.get(&kappa).unwrap().as_deref(),
            Some(content.as_slice()),
            "content survives a node restart"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The node serves its store over HTTP-CAS, and a content peer fetches a κ
    /// over `GET /cas/{κ}` and verifies it on receipt — the same path the browser
    /// peer's `fetch()` + `receive` takes.
    #[test]
    fn serves_content_over_http_cas_verified_on_receipt() {
        let dir = temp_store_dir("serve");
        let store = open_store(&dir).unwrap();
        let content = b"a layer the node serves to a browser tab";
        let kappa = store.put("blake3", content).unwrap();

        let server = serve(store, "127.0.0.1:0").expect("serve HTTP-CAS");
        let url = format!("http://{}{}", server.addr(), cas_path(&kappa));

        // A peer fetches by κ (a plain GET — what the browser's fetch() does).
        let resp = ureq::get(&url).call().expect("fetch /cas/{κ}");
        let mut bytes = Vec::new();
        resp.into_reader().read_to_end(&mut bytes).unwrap();

        assert_eq!(bytes, content, "the node served the content");
        // Verify on receipt (Law L5): the bytes re-derive to the κ requested.
        assert!(
            verify_kappa(&bytes, &kappa).unwrap(),
            "served content re-derives to its κ (the node cannot forge)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
