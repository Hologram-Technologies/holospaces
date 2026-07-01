//! `CC-76` (Polish-3) — the open(κ) page store is served PEER-TO-PEER, not just from a static host.
//!
//! open(κ) resumes a warm image by fetching its unique 4 KiB RAM pages BY κ (Polish-1). Those pages are
//! plain content-addressed blobs, so the SAME κ that a tab fetches over HTTP can be fetched from another
//! PEER over the κ-native content network (`content_net` — the identical code the wasm tab drives over a
//! WebRTC data channel, `cc38`/`cc49`). This witnesses that a real open(κ) page crosses two peers by κ,
//! is **verified on receipt** (L5 — re-derives to the requested κ), and that a **forging** peer serving
//! the wrong bytes for that κ is **refused** — so a page store can be distributed peer-to-peer with no
//! trusted host, the last "100% serverless" seam.

use std::sync::Arc;

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::address;
use holospaces::content_net::{drive_fetch, forging_peer, peer, PacketLink};

/// A representative open(κ) page: a 4 KiB RAM page (the exact unit `resume_kappa_streamed` fetches by κ).
fn a_kappa_page() -> Vec<u8> {
    // Deterministic, non-trivial bytes (as a real guest RAM page would be) so the κ is a real digest.
    (0..4096u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect()
}

#[test]
fn an_open_kappa_page_crosses_two_peers_by_kappa_verified() {
    let page = a_kappa_page();

    // The PUBLISHER peer holds the page (as a tab that sealed a warm image would); the ADOPTER peer — a
    // fresh tab opening the κ-link — holds nothing. BOTH are the identical `content_net::peer` (the same
    // code the browser and a bare-metal board run), so they interoperate by construction.
    let publisher_store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());
    let kappa = publisher_store.put("blake3", &page).unwrap();
    let adopter_store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());

    let (link_pub, link_adopt) = PacketLink::loopback_pair(256 * 1024);
    let publisher = peer(link_pub, publisher_store);
    let adopter = peer(link_adopt, adopter_store);

    // The adopter fetches the page BY κ from the publisher over content_net — no static host.
    let got = drive_fetch(&adopter, &publisher, &kappa).expect("adopter fetched the page by κ from a peer");
    assert_eq!(&got[..], &page[..], "the peer-fetched page is byte-identical");
    // Verify-on-receipt (L5): the bytes re-derive to the requested κ — a lie can't survive this.
    assert_eq!(address(&got[..]), kappa, "the fetched page re-derives to its κ");

    // A κ no peer holds settles as absent (no forging, no hang).
    let absent = address(b"a page no peer holds");
    assert!(drive_fetch(&adopter, &publisher, &absent).is_none(), "an unheld κ is absent");
}

#[test]
fn a_forging_peer_serving_the_wrong_page_is_refused() {
    let page = a_kappa_page();
    let kappa = address(&page[..]); // the κ the adopter asks for

    // A malicious peer that answers EVERY κ request with different bytes — the untrusted-host threat.
    let forged = {
        let mut f = page.clone();
        f[0] ^= 0xff; // one flipped bit
        f
    };
    let (link_adopt, link_forge) = PacketLink::loopback_pair(256 * 1024);
    let adopter = peer(link_adopt, Arc::new(MemKappaStore::new()) as Arc<dyn KappaStore>);
    let forger = forging_peer(link_forge, forged);

    // The adopter asks for the honest κ; the forger serves tampered bytes → REFUSED (they don't re-derive
    // to the κ). This is what makes a peer-served page store safe with no trusted host (Law L5).
    assert!(
        drive_fetch(&adopter, &forger, &kappa).is_none(),
        "a forging peer's tampered page must be refused, not accepted"
    );
}
