//! `CC-38` — the uor-native content network ("the browser as a router").
//!
//! A peer fetches another peer's content the same content-addressed way on every
//! deployment surface. The substrate supplies the mechanism
//! (`hologram-net-bare`'s `BareNetSync` over a `NetworkInterface`); holospaces
//! supplies a portable `NetworkInterface` (`content_net::PacketLink`) so the
//! **same** sync runs in a browser tab (wasm), on a bare-metal board
//! (`thumbv7em-none-eabi`), and on a native host.
//!
//! This witnesses that a **browser peer and a bare-metal peer interoperate** —
//! both are built by the *identical* `content_net::peer` over the *identical*
//! `BareNetSync` and frame codec, so they speak the same wire protocol by
//! construction. Here they exchange content over an in-process `PacketLink` pair
//! (the loopback stands in for the live transport — a WebRTC data channel
//! between tabs, a real NIC on bare metal); the browser surface drives the same
//! path in `holospaces-web` (`Console::content_network_selftest`, witnessed in
//! Chromium) and the bare-metal build gate compiles it for `thumbv7em`.

use std::sync::Arc;

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::KappaStore;
use holospaces::address;
use holospaces::content_net::{drive_fetch, drive_fetch_over_transport, peer, PacketLink};

/// A content store holding `content`, with the κ that addresses it.
fn store_with(content: &[u8]) -> (Arc<dyn KappaStore>, holospaces::Kappa) {
    let store = MemKappaStore::new();
    let kappa = store.put("blake3", content).unwrap();
    (Arc::new(store), kappa)
}

/// A browser peer fetches content it does not hold from a bare-metal peer over
/// the uor-native network, and the bytes are verified on receipt.
#[test]
fn a_browser_peer_fetches_content_from_a_bare_metal_peer() {
    // The "bare-metal" peer holds the content; the "browser" peer is empty. BOTH
    // are built by the identical `content_net::peer` over the identical
    // `BareNetSync` — the same code the wasm tab and the thumbv7em board run — so
    // they interoperate by construction (not by a shared adapter, but by being
    // the same implementation).
    let content = b"a layer blob, addressed by content, routed between peers";
    let (bare_store, kappa) = store_with(content);
    let browser_store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());

    let (link_bare, link_browser) = PacketLink::loopback_pair(256 * 1024);
    let bare = peer(link_bare, bare_store);
    let browser = peer(link_browser, browser_store);

    let got = drive_fetch(&browser, &bare, &kappa)
        .expect("the browser peer fetched the content from the bare-metal peer");
    assert_eq!(&got[..], &content[..], "the fetched bytes are the content");
    // Verify-on-receipt (SPINE-4 / Law L5): the bytes re-derive to the κ that was
    // requested — a forging responder would be rejected inside BareNetSync.
    assert_eq!(
        address(&got[..]),
        kappa,
        "the fetched content re-derives to the requested κ"
    );

    // A κ that neither peer holds resolves to nothing — no forging, no false
    // content, no hang (the exchange settles, then reports absence).
    let unheld = address(b"content that no peer holds");
    assert!(
        drive_fetch(&browser, &bare, &unheld).is_none(),
        "a κ no peer holds is absent"
    );
}

/// The network is symmetric — there are no client/server roles. The reverse
/// direction (a bare-metal peer fetching from a browser peer) works identically.
#[test]
fn the_content_network_is_bidirectional() {
    let content = b"content held by the browser peer, fetched by the bare-metal peer";
    let (browser_store, kappa) = store_with(content);
    let bare_store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());

    let (link_browser, link_bare) = PacketLink::loopback_pair(256 * 1024);
    let browser = peer(link_browser, browser_store);
    let bare = peer(link_bare, bare_store);

    let got = drive_fetch(&bare, &browser, &kappa)
        .expect("the bare-metal peer fetched the content from the browser peer");
    assert_eq!(&got[..], &content[..]);
    assert_eq!(address(&got[..]), kappa);
}

/// The transport seam: two peers carry the protocol over `TransportEndpoint`s
/// (the handle a WebRTC data channel pump bridges between browser tabs), not a
/// direct in-process pairing. This is the exact path the browser peer drives —
/// the link is the same `PacketLink`, only the carrier (a real data channel vs.
/// this in-test wire) differs — so the deployed transport rides a proven seam.
#[test]
fn peers_exchange_content_over_the_transport_seam() {
    let content = b"content routed over the transport seam (a WebRTC data channel)";
    let (a_store, kappa) = store_with(content);
    let b_store: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());

    let (a_link, a_wire) = PacketLink::with_transport(256 * 1024);
    let (b_link, b_wire) = PacketLink::with_transport(256 * 1024);
    let peer_a = peer(a_link, a_store);
    let peer_b = peer(b_link, b_store);

    // B fetches A's content; the pump carries frames over the endpoints.
    let got = drive_fetch_over_transport(&peer_b, &b_wire, &peer_a, &a_wire, &kappa)
        .expect("content fetched over the transport seam");
    assert_eq!(&got[..], &content[..]);
    assert_eq!(
        address(&got[..]),
        kappa,
        "verified on receipt over the seam"
    );
}
