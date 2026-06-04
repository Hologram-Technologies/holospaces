//! **OTA — the node updates itself from the holospaces Pages site.**
//!
//! All nodes look to the holospaces GitHub Pages site for updates and install
//! them as they become available. The site is the *cold-start gateway* (ADR-005)
//! — untrusted, like any CDN — so the node does not trust it: a node update is
//! **κ-addressed**, and the node **verifies it by re-derivation** (Law L5) before
//! staging it. A tampered or substituted update does not re-derive to the κ the
//! manifest names and is **refused**, so a compromised CDN cannot push malicious
//! firmware to the fleet.
//!
//! The flow is the substrate's content read, nothing bespoke: read the update κ
//! the site advertises (a manifest), fetch the artifact by that κ from the site's
//! `/cas/{κ}`, verify, and stage it for the next restart to adopt.

use std::io::Read;
use std::path::Path;

use hologram_substrate_core::{verify_kappa, KappaLabel71};

/// Why an OTA update could not be staged.
#[derive(Debug)]
pub enum OtaError {
    /// The fetch failed (transport, DNS, TLS, HTTP status).
    Fetch(String),
    /// The fetched bytes did **not** re-derive to the κ the manifest named — a
    /// tampered or substituted update, refused (Law L5).
    Forged,
    /// Writing the staged update to disk failed.
    Io(String),
}

impl std::fmt::Display for OtaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OtaError::Fetch(e) => write!(f, "OTA fetch failed: {e}"),
            OtaError::Forged => write!(
                f,
                "OTA update does not re-derive to its κ — refused (Law L5)"
            ),
            OtaError::Io(e) => write!(f, "OTA staging write failed: {e}"),
        }
    }
}

impl std::error::Error for OtaError {}

/// Fetch a κ-addressed artifact from `url` and **verify it re-derives to
/// `expected`** (Law L5). Returns the verified bytes, or [`OtaError::Forged`] if
/// the content does not match the κ — the trust boundary the cold-start gateway
/// is held to.
pub fn fetch_verified(url: &str, expected: &KappaLabel71) -> Result<Vec<u8>, OtaError> {
    let resp = ureq::get(url)
        .call()
        .map_err(|e| OtaError::Fetch(e.to_string()))?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| OtaError::Fetch(e.to_string()))?;
    if !verify_kappa(&bytes, expected).map_err(|_| OtaError::Forged)? {
        return Err(OtaError::Forged);
    }
    Ok(bytes)
}

/// Fetch + verify + **stage** a node update: the update κ (`expected`, from the
/// site's manifest) is fetched from `url` (the site's `/cas/{κ}`), verified by
/// re-derivation, and written to `staging` for the next restart to adopt. A
/// forged update never reaches `staging`.
pub fn stage_update(url: &str, expected: &KappaLabel71, staging: &Path) -> Result<(), OtaError> {
    let bytes = fetch_verified(url, expected)?;
    std::fs::write(staging, &bytes).map_err(|e| OtaError::Io(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage;
    use hologram_net_http::cas_path;
    use hologram_store_mem::MemKappaStore;
    use hologram_substrate_core::KappaStore;
    use std::sync::Arc;

    /// The node fetches a κ-addressed update from a (cold-start) CAS gateway,
    /// verifies it re-derives, and stages it.
    #[test]
    fn fetches_verifies_and_stages_an_update() {
        let store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());
        let update = b"a newer holospaces-node build, addressed by its content";
        let kappa = store.put("blake3", update).unwrap();

        // The site serves the update over /cas/{κ} (the cold-start gateway).
        let site = storage::serve(store, "127.0.0.1:0").expect("serve the site");
        let url = format!("http://{}{}", site.addr(), cas_path(&kappa));

        let staging = std::env::temp_dir().join(format!("hsn-ota-{}.staged", std::process::id()));
        let _ = std::fs::remove_file(&staging);
        stage_update(&url, &kappa, &staging).expect("stage the verified update");
        assert_eq!(
            std::fs::read(&staging).unwrap(),
            update,
            "the verified update is staged for the next restart"
        );
        let _ = std::fs::remove_file(&staging);
    }

    /// A forged update — content that does not re-derive to the κ the manifest
    /// names — is refused, and never reaches staging.
    #[test]
    fn a_forged_update_is_refused() {
        let store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());
        let real = store.put("blake3", b"the real update").unwrap();
        // The κ the manifest claims, but the site (compromised) serves OTHER bytes.
        let lie = store
            .put("blake3", b"a malicious payload a CDN tries to push")
            .unwrap();

        let site = storage::serve(store, "127.0.0.1:0").expect("serve the site");
        // Ask for the malicious bytes but claim they are the real update's κ.
        let url = format!("http://{}{}", site.addr(), cas_path(&lie));
        match fetch_verified(&url, &real) {
            Err(OtaError::Forged) => {}
            other => panic!("a forged update must be refused, got {other:?}"),
        }
    }
}
