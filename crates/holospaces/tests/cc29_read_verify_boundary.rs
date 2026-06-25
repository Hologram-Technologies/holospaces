//! `CC-29` — L5 verification is placed at the trust boundary, not charged as a
//! per-read tax on the deployed peer (arc42 ch.10; ADR-019).
//!
//! Law L5 says content is accepted only when its bytes re-derive to the
//! requested κ. The architectural question is *where* that check is spent. The
//! substrate's own read path verifies **on receipt** at an untrusted gateway
//! (`get_with_fetch`; the real loopback HTTP-CAS gateway in the `CC-28` / e2e
//! witnesses) and the OCI ingestor verifies every blob against its `sha256`
//! digest on the way in (`CC-10`, `verify_kappa_axis`). Once content is in
//! *this* peer's own in-session store, the store **is** the canonical memory and
//! RAM is its cache (Law L3): re-deriving it on every local read would treat the
//! canonical store as untrusted — the opposite of the model — and is pure
//! overhead in the deployed browser peer.
//!
//! Authority/oracle: the substrate's own `verify_kappa` re-derivation contract —
//! the exact primitive `get_with_fetch` and the OCI ingestor run at the receipt
//! boundary. The threat is modelled in miniature by a *forging store* that lies
//! about one κ (returns bytes that do not re-derive to it), the same way an
//! untrusted gateway can serve any bytes.
//!
//! Witness:
//!   * the boundary check ([`ReadVerify::OnRead`]) rejects the liar — L5 holds
//!     wherever untrusted bytes can enter;
//!   * the trusted in-session read ([`ReadVerify::Trusted`]) returns the
//!     canonical store's bytes without re-deriving (Law L3) — the deployed
//!     peer's read;
//!   * honest content reads identically under both policies (the policy only
//!     changes whether the boundary check is re-run, never the result for
//!     content that is what it claims to be);
//!   * `resolve_local`'s default is the verifying boundary policy (the safe
//!     default for a general caller; an in-session peer opts into `Trusted`).

use hologram_store_mem::MemKappaStore;
use hologram_substrate_core::{AccessError, Bytes, KappaLabel71, KappaStore, StoreError};
use holospaces::boot::{ReadVerify, Resolver};
use holospaces::realizations::address;

/// A store that lies about exactly one κ: `get(forged_key)` returns bytes that
/// do **not** re-derive to it. Everything else delegates to a real
/// `MemKappaStore`. This is the untrusted-gateway threat in miniature — the
/// bytes a gateway serves for a κ may be anything (CC-10/CC-28 face the same
/// threat over a real HTTP-CAS gateway).
struct ForgingStore {
    inner: MemKappaStore,
    forged_key: KappaLabel71,
    forged_bytes: Bytes,
}

impl ForgingStore {
    /// A gateway that *claims* to serve `claimed` under its κ but actually
    /// returns `actually`.
    fn new(claimed: &[u8], actually: &[u8]) -> Self {
        Self {
            inner: MemKappaStore::new(),
            forged_key: address(claimed),
            forged_bytes: Bytes::from(actually.to_vec()),
        }
    }
}

impl KappaStore for ForgingStore {
    fn put(&self, axis: &str, canonical_bytes: &[u8]) -> Result<KappaLabel71, StoreError> {
        self.inner.put(axis, canonical_bytes)
    }
    fn get(&self, kappa: &KappaLabel71) -> Result<Option<Bytes>, StoreError> {
        if kappa == &self.forged_key {
            return Ok(Some(self.forged_bytes.clone()));
        }
        self.inner.get(kappa)
    }
    fn contains(&self, kappa: &KappaLabel71) -> bool {
        kappa == &self.forged_key || self.inner.contains(kappa)
    }
    fn pin(&self, kappa: &KappaLabel71) -> Result<(), StoreError> {
        self.inner.pin(kappa)
    }
    fn unpin(&self, kappa: &KappaLabel71) -> Result<(), StoreError> {
        self.inner.unpin(kappa)
    }
    fn iterate(&self) -> Vec<KappaLabel71> {
        self.inner.iterate()
    }
    fn pinned_roots(&self) -> Vec<KappaLabel71> {
        self.inner.pinned_roots()
    }
    fn approximate_count(&self) -> usize {
        self.inner.approximate_count()
    }
    fn approximate_bytes(&self) -> u64 {
        self.inner.approximate_bytes()
    }
}

#[test]
fn the_boundary_check_rejects_a_liar_but_the_trusted_read_trusts_the_store() {
    // A gateway claims to serve one thing and returns another.
    let store = ForgingStore::new(
        b"the content the gateway claims to serve",
        b"tampered bytes the gateway actually served",
    );
    let claimed_kappa = store.forged_key;

    // OnRead is the L5 boundary check — the same `verify_kappa` re-derivation
    // `get_with_fetch` and the OCI ingestor run on receipt. The liar is refused.
    assert_eq!(
        Resolver::resolve_local_with(&store, &claimed_kappa, ReadVerify::OnRead),
        Err(AccessError::VerificationFailed),
        "the trust-boundary read must reject bytes that do not re-derive to κ (L5)"
    );

    // Trusted trusts the canonical store (Law L3): it returns the bytes as-is,
    // no re-derivation. This is exactly why Trusted may only wrap content that
    // was already verified on entry — it is the deployed peer's in-session read,
    // not a boundary read.
    assert_eq!(
        Resolver::resolve_local_with(&store, &claimed_kappa, ReadVerify::Trusted)
            .unwrap()
            .as_deref(),
        Some(&b"tampered bytes the gateway actually served"[..]),
        "the trusted read returns the store's bytes as-is (does not reject the forged blob)"
    );
}

#[test]
fn honest_in_session_content_reads_identically_under_both_policies() {
    // Content placed into this peer's own store (κ-addressed by `put`).
    let store = MemKappaStore::new();
    let bytes = b"a holospace part placed into this peer's own canonical store";
    let k = store.put("blake3", bytes).unwrap();

    let onread = Resolver::resolve_local_with(&store, &k, ReadVerify::OnRead).unwrap();
    let trusted = Resolver::resolve_local_with(&store, &k, ReadVerify::Trusted).unwrap();

    assert_eq!(onread.as_deref(), Some(&bytes[..]));
    assert_eq!(trusted.as_deref(), Some(&bytes[..]));
    assert_eq!(
        onread, trusted,
        "for content that is what it claims to be, the policy changes only whether \
         the boundary check is re-run — never the result"
    );
}

#[test]
fn resolve_local_default_is_the_verifying_boundary_policy() {
    // The default `resolve_local` is `OnRead` — the safe boundary default for a
    // general caller. An in-session peer that trusts its own canonical store
    // opts into `Trusted` explicitly (ADR-019).
    let store = ForgingStore::new(b"claimed content", b"a lie");
    let claimed_kappa = store.forged_key;
    assert_eq!(
        Resolver::resolve_local(&store, &claimed_kappa),
        Err(AccessError::VerificationFailed),
        "resolve_local defaults to the verifying boundary policy"
    );
}

#[test]
fn an_absent_kappa_resolves_to_none_under_both_policies() {
    // Neither policy invents content: an absent κ is `None`, not an error.
    let store = MemKappaStore::new();
    let absent = address(b"content this peer never stored");
    assert!(
        Resolver::resolve_local_with(&store, &absent, ReadVerify::OnRead)
            .unwrap()
            .is_none()
    );
    assert!(
        Resolver::resolve_local_with(&store, &absent, ReadVerify::Trusted)
            .unwrap()
            .is_none()
    );
}
