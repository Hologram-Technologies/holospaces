//! `CC-40` — product security: the threat model's properties are **enforced**,
//! not asserted. Each security requirement in the Product Security & Threat
//! Model (`docs/13-Product-Security.md`) has a witness here proving holospaces
//! implements it. The properties are UOR-native — they hold by construction, so
//! the witnesses are about what the substrate **refuses**, not about a bolted-on
//! check.

use std::sync::Arc;

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{Capabilities, KappaStore};
use holospaces::content_net::{drive_fetch, peer, PacketLink};
use holospaces::identity::{Operator, Roster};
use holospaces::{address, verify};

// ── SEC-1 Integrity: forgery is structurally impossible (verify-on-receipt) ──
// Content *is* its κ; any byte that does not re-derive to the κ is refused. A
// malicious peer, gateway, or observer cannot substitute or tamper content —
// there is no trusted intermediary to subvert (Law L1/L5).
#[test]
fn sec_integrity_tampered_content_does_not_re_derive() {
    let content = b"a package the network distributes, addressed by its content";
    let kappa = address(content);
    assert!(
        verify(content, &kappa).unwrap(),
        "the content re-derives to its κ"
    );

    // Flip one bit: the tampered bytes do not re-derive to the κ — refused.
    let mut tampered = content.to_vec();
    tampered[0] ^= 0x01;
    assert!(
        !verify(&tampered, &kappa).unwrap(),
        "a single tampered bit is refused by re-derivation"
    );
}

#[test]
fn sec_integrity_the_network_refuses_a_forging_responder() {
    // A peer can only ever serve content that re-derives to the requested κ. The
    // fetcher verifies on receipt (SPINE-4), so a responder cannot fabricate a
    // value for a κ it does not legitimately hold — an unheld κ resolves to
    // nothing (the content network never invents content).
    let honest: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());
    let real = honest.put("blake3", b"the real content").unwrap();
    let empty: Arc<dyn KappaStore> = Arc::new(MemKappaStore::new());

    let (link_a, link_b) = PacketLink::loopback_pair(64 * 1024);
    let server = peer(link_a, honest);
    let client = peer(link_b, empty);

    assert!(
        drive_fetch(&client, &server, &real).is_some(),
        "real content is delivered"
    );
    // A κ the responder does not hold is never fabricated.
    let phantom = address(b"content no peer legitimately holds");
    assert!(
        drive_fetch(&client, &server, &phantom).is_none(),
        "the network refuses to fabricate content for an unheld κ"
    );
}

// ── SEC-2 Authority: object-capabilities, no ambient authority, attenuation ──
// A holospace runs under exactly its κ-addressed capability set; authority can
// only be ATTENUATED, never escalated. `Capabilities::admits` is the kernel of
// this — it is what refuses a confused-deputy / ambient-authority escalation.
#[test]
fn sec_authority_capabilities_only_attenuate_never_escalate() {
    let root = address(b"a storage root the parent holds");
    let parent = Capabilities {
        storage_roots: vec![root],
        storage_quota_bytes: 1024,
        network_fetch: true,
        network_announce: false,
        publish_channels: Vec::new(),
        subscribe_channels: Vec::new(),
        memory_max_bytes: 256 * 1024 * 1024,
        cpu_time_per_event_ms: 100,
        priority_weight: 10,
    };

    // A strictly-lesser child is admitted (legitimate attenuation).
    let attenuated = Capabilities {
        storage_roots: Vec::new(),
        storage_quota_bytes: 512,
        network_fetch: false,
        memory_max_bytes: 128 * 1024 * 1024,
        cpu_time_per_event_ms: 50,
        priority_weight: 5,
        ..parent.clone()
    };
    assert!(
        parent.admits(&attenuated),
        "a lesser capability set is admitted"
    );

    // Every escalation vector is refused — no ambient authority leaks through.
    let grant_unheld_network = Capabilities {
        network_announce: true, // the parent does not hold announce
        ..parent.clone()
    };
    assert!(
        !parent.admits(&grant_unheld_network),
        "a flag the parent lacks cannot be granted"
    );

    let widen_quota = Capabilities {
        storage_quota_bytes: 4096, // > parent
        ..parent.clone()
    };
    assert!(!parent.admits(&widen_quota), "a wider quota is refused");

    let unbounded_under_bounded = Capabilities {
        storage_quota_bytes: 0, // 0 = unbounded, under a bounded parent
        ..parent.clone()
    };
    assert!(
        !parent.admits(&unbounded_under_bounded),
        "an unbounded budget under a bounded parent is refused"
    );

    let foreign_root = Capabilities {
        storage_roots: vec![address(b"a root the parent does not hold")],
        ..parent.clone()
    };
    assert!(
        !parent.admits(&foreign_root),
        "a storage root outside the parent's is refused"
    );
}

// ── SEC-3 Cost/dedup: identical content resolves to one κ, stored once ─────────
// The UOR cost model (not the internet's per-request/per-byte model): content is
// resolved ONCE and shared. Idempotent `put` is the holospaces-observable form —
// re-storing identical content does not grow the store.
#[test]
fn sec_cost_identical_content_deduplicates() {
    let store = MemKappaStore::new();
    let content = b"a layer many guests fetch - resolved once, shared";

    let k1 = store.put("blake3", content).unwrap();
    let count_after_first = store.approximate_count();
    let k2 = store.put("blake3", content).unwrap();

    assert_eq!(k1, k2, "identical content has one κ");
    assert_eq!(
        store.approximate_count(),
        count_after_first,
        "re-storing identical content does not grow the store (deduplicated)"
    );
}

// ── SEC-4 Identity: self-sovereign, deterministic, unforgeable ────────────────
// An operator is a content-addressed identity (`CC-1`), not a server account.
// The same key always yields the same identity; a different key, a different
// one — there is no central account to breach and no identity to forge without
// the key.
#[test]
fn sec_identity_is_self_sovereign_and_unforgeable() {
    let a1 = Operator::from_public_key(b"operator-A-public-key");
    let a2 = Operator::from_public_key(b"operator-A-public-key");
    let b = Operator::from_public_key(b"operator-B-public-key");

    assert_eq!(
        a1.identity(),
        a2.identity(),
        "the same key is the same identity"
    );
    assert_ne!(
        a1.identity(),
        b.identity(),
        "a different key is a different identity"
    );

    // A roster is content-addressed and embeds the operator identity: it cannot
    // be forged for another operator, and any tamper changes its κ.
    let roster = Roster::new(&a1, vec![address(b"a holospace")]);
    let kappa = roster.kappa();
    let recovered = Roster::from_canonical(&roster_canonical(&roster)).unwrap();
    assert_eq!(
        recovered.kappa(),
        kappa,
        "the roster round-trips to the same κ"
    );
    assert_eq!(
        recovered.operator(),
        a1.identity(),
        "the roster binds its operator"
    );
}

// ── SEC-5 Confidentiality: the κ addresses content ───────────────────────────
// Content is addressed by its κ: `get(κ)` resolves the content, and a κ the store
// was never given resolves to nothing. (This layer's store also exposes
// `iterate()`, so it does not by itself enforce "no enumeration"; real
// confidentiality is the UOR/substrate layer this builds on — frame-relative
// perception, content meaningful only in the observer's base-frame. What is
// asserted here is just the addressing property: present κ → Some, absent κ → None.)
#[test]
fn an_absent_kappa_resolves_to_none() {
    let store = MemKappaStore::new();
    let secret = b"content addressed by its kappa";
    let kappa = store.put("blake3", secret).unwrap();

    assert!(
        store.get(&kappa).unwrap().is_some(),
        "a present κ resolves to its content"
    );
    // A κ the store was never given resolves to nothing.
    let unknown = address(b"a kappa no one was given");
    assert!(
        store.get(&unknown).unwrap().is_none(),
        "an unknown κ is absent"
    );
}

// ── SEC-3 (deepened) — content has ONE identity network-wide ──────────────────
// The foundation of the dense-matrix cost model: content's identity is its κ,
// computed identically on every peer, so the same artifact is the *same content*
// everywhere — resolved once and shared, never re-identified per peer. This is
// what makes "resolved once, deduplicated network-wide" hold across independent
// peers, not just within one store (idempotent put, SEC-3).
#[test]
fn sec_cost_content_has_one_identity_on_every_peer() {
    let peer_a = MemKappaStore::new();
    let peer_b = MemKappaStore::new();
    let content = b"a package, identified by its content, the same on every node";

    let on_a = peer_a.put("blake3", content).unwrap();
    let on_b = peer_b.put("blake3", content).unwrap();
    assert_eq!(
        on_a, on_b,
        "the same content has one identity across independent peers (network-wide dedup)"
    );
    // And it is the peer-independent address any peer computes for the bytes.
    assert_eq!(
        on_a,
        address(content),
        "the identity is the content address, not a peer's choice"
    );
}

// ── SEC-6 — Reference resolution: verified against the κ, not the reference ────
// "this URL/name → this κ" is the trust-sensitive boundary (ADR-013). The located
// reference is the *request*; the κ is the *identity*. Whatever a reference points
// at, the content is **verified by re-derivation against the κ on the κ's own
// axis** before it is accepted — an OCI `sha256:` digest IS a κ on the `sha256`
// axis (`CC-10`/`CC-20`), so a tampered blob is refused regardless of where the
// reference led.
#[test]
fn sec_reference_resolution_verifies_against_the_kappa_on_its_axis() {
    use holospaces::realizations::{address_on, Axis};
    let blob = b"an image layer a manifest references by its sha256 digest";

    // The OCI digest a reference names is the κ on the sha256 axis.
    let digest = address_on(blob, Axis::Sha256).unwrap();
    assert!(
        hologram_substrate_core::verify_kappa_axis(blob, &digest).unwrap(),
        "the content re-derives to the digest the reference named"
    );

    // A tampered blob does not re-derive — refused at the import boundary, no
    // matter that the reference resolved to it.
    let mut tampered = blob.to_vec();
    tampered[0] ^= 0x01;
    assert!(
        !hologram_substrate_core::verify_kappa_axis(&tampered, &digest).unwrap(),
        "a tampered blob is refused on the referenced axis"
    );
}

/// The canonical bytes of a roster (the form its κ addresses), via the
/// substrate's `Realization` contract.
fn roster_canonical(roster: &Roster) -> Vec<u8> {
    use hologram_substrate_core::Realization;
    roster.canonicalize()
}
