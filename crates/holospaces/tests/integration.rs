//! Integration tests (the *integration* tier).
//!
//! These exercise the building blocks of arc42 chapter 5
//! (`docs/src/arc42/adoc/05_building_block_view.adoc`) *composed* — the Boot
//! Layer over Realizations over the hologram substrate (real `KappaStore`) —
//! against the quality scenarios of arc42 chapter 10. CI runs this tier via
//! `cargo test --workspace --test integration`.

use hologram_store_mem::MemKappaStore;
use holospaces::boot::{ingest_devcontainer, Resolver};
use holospaces::substrate::{KappaStore, Realization};
use holospaces::Capabilities;

const CONFIG: &[u8] = br#"{"name":"app","image":"debian:12"}"#;

fn userland() -> holospaces::Kappa {
    holospaces::address(b"the recompiled userland this devcontainer selects")
}

fn caps() -> Capabilities {
    Capabilities {
        storage_roots: Vec::new(),
        storage_quota_bytes: 0,
        network_fetch: false,
        network_announce: false,
        publish_channels: Vec::new(),
        subscribe_channels: Vec::new(),
        memory_max_bytes: 0,
        cpu_time_per_event_ms: 0,
        priority_weight: 0,
    }
}

fn provision() -> holospaces::Holospace {
    ingest_devcontainer(
        "https://example.invalid/app.git",
        "main",
        ".devcontainer/devcontainer.json",
        CONFIG,
        userland(),
        caps(),
    )
    .expect("ingest")
}

/// Ingest → store the canonical form → resolve (verify by re-derivation) →
/// the stored κ is the holospace identity (Laws L2/L5).
#[test]
fn ingest_store_and_resolve_a_holospace() {
    let hs = provision();
    let canonical = hs.canonicalize();

    let store = MemKappaStore::new();
    let kappa = store.put("blake3", &canonical).unwrap();
    assert_eq!(kappa, hs.kappa(), "stored κ is the holospace identity");

    let resolved = Resolver::resolve_local(&store, &kappa)
        .expect("resolve")
        .expect("present");
    assert_eq!(resolved.as_ref(), canonical.as_slice());
}

/// QS1: the same git repo + devcontainer provisioned twice, on different peers,
/// yields the same holospace κ.
#[test]
fn same_definition_on_two_peers_yields_one_kappa() {
    let store_a = MemKappaStore::new();
    let store_b = MemKappaStore::new();
    let k_a = store_a.put("blake3", &provision().canonicalize()).unwrap();
    let k_b = store_b.put("blake3", &provision().canonicalize()).unwrap();
    assert_eq!(k_a, k_b);
}

/// The capability set is part of the reproducible definition: change the
/// authority, change the identity (arc42 chapter 8, *Capabilities*).
#[test]
fn capability_change_changes_identity_end_to_end() {
    let open = provision();
    let mut scoped_caps = caps();
    scoped_caps.memory_max_bytes = 512 << 20;
    scoped_caps.network_fetch = true;
    let scoped = ingest_devcontainer(
        "https://example.invalid/app.git",
        "main",
        ".devcontainer/devcontainer.json",
        CONFIG,
        userland(),
        scoped_caps,
    )
    .unwrap();
    assert_ne!(open.kappa(), scoped.kappa());
}

/// QS3: a peer that serves bytes not matching the requested κ is rejected on
/// re-derivation; the embedded operand κ-labels (manifest, capabilities) are
/// recoverable from the verified canonical form (SPINE-3).
#[test]
fn resolution_verifies_and_references_are_recoverable() {
    let hs = provision();
    let canonical = hs.canonicalize();
    let store = MemKappaStore::new();
    let kappa = store.put("blake3", &canonical).unwrap();

    let bytes = Resolver::resolve_local(&store, &kappa).unwrap().unwrap();
    let refs = holospaces::Holospace::references(&bytes).unwrap();
    assert_eq!(refs, vec![*hs.manifest(), *hs.capabilities()]);

    // A κ the store does not honestly hold resolves to nothing (no forgery).
    let forged = holospaces::address(b"content the store never put");
    assert!(Resolver::resolve_local(&store, &forged).unwrap().is_none());
}
