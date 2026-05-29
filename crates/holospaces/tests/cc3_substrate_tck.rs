//! **CC-3 — A peer's storage obeys the substrate contract.**
//!
//! The Conformance catalog row `CC-3` (arc42 chapter 10,
//! `docs/src/arc42/adoc/10_quality_requirements.adoc`): a peer's storage obeys
//! the substrate contract, witnessed by running the
//! [hologram](https://github.com/Hologram-Technologies/hologram) substrate
//! conformance battery (TCK) — the external authority — against the stores
//! holospaces resolves through.
//!
//! holospaces does not implement storage; it resolves through hologram's
//! `KappaStore` (Law L4, ADR-006). This witness runs the imported
//! `hologram_substrate_tck::store_battery` against both reference stores
//! holospaces uses — the in-memory store (browser/transient peers, and the
//! holospaces `Resolver` tests) and the native redb store (native peers).
//! `store_battery` panics on the first conformance violation.
//!
//! Run by `vv/run.sh`; also `cargo test -p holospaces --test cc3_substrate_tck`.

use hologram_store_mem::MemKappaStore;
use hologram_store_native::NativeKappaStore;
use hologram_substrate_tck::store_battery;

/// The in-memory store obeys the `KappaStore` contract (all conformance
/// points: idempotency, eviction-tolerant get, fail-loud unknown axis,
/// pin/unpin, content round-trip + re-derivation, axis-polymorphism,
/// zero-copy).
#[test]
fn in_memory_store_obeys_the_substrate_contract() {
    store_battery(&MemKappaStore::new());
}

/// The native (redb) store obeys the `KappaStore` contract.
#[test]
fn native_store_obeys_the_substrate_contract() {
    let store = NativeKappaStore::in_memory().expect("open in-memory native store");
    store_battery(&store);
}
