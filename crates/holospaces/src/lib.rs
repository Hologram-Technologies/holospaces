//! # holospaces
//!
//! UOR-native boot layer over the [hologram](https://github.com/Hologram-Technologies/hologram)
//! substrate. holospaces provisions and runs *holospaces* — bootable,
//! content-addressed environments — and ships the Hologram Platform Manager.
//!
//! The documentation in `docs/` is authoritative (ADR-005); this code traces
//! back to it. The building blocks defined by the architecture (arc42 chapter
//! 5, `docs/src/arc42/adoc/05_building_block_view.adoc`) map to the modules
//! here, each a thin composition of the hologram substrate, consumed by
//! reference (ADR-003, ADR-006) — never re-implemented:
//!
//! - [`realizations`] — holospaces' canonical-form layer: κ-labels
//!   ([`Kappa`] = `hologram_substrate_core::KappaLabel71`) minted and verified
//!   by re-derivation through the substrate (Laws L1, L2, L3, L5), and the
//!   [`Holospace`] — a hologram `Realization` composing a `ContainerManifest`
//!   and a `CapabilitySet`. Conformance: `CC-1`.
//! - [`boot`] — the environment-agnostic core: ingest, `provision` (κ-address a
//!   holospace's parts into the store), resolve (the substrate's
//!   verify-on-receipt read), and a [`Session`](boot::Session) driving the
//!   lifecycle through hologram's `ContainerRuntime`.
//! - [`engine`] — the *.holo Engine*: runs a `.holo` compute artifact via the
//!   hologram executor. Conformance: `CC-2`.
//! - [`surface`] — the *Execution Surface*: the κ-addressed Wasm-recompiled
//!   userland form (the second compute form) and the host-ABI contract it must
//!   bind (ADR-008, resolving RT1). Conformance: `CC-6`.
//! - [`peer`] — a [`Peer`](peer::Peer) that composes the substrate for an
//!   environment (storage · network · runtime) and supplies the boot
//!   operations, incl. reachability-closure migration (arc42 chapter 7).
//! - [`identity`] — self-sovereign sign-in ([`Operator`](identity::Operator))
//!   and the [`Roster`](identity::Roster) that links an operator's instances so
//!   their holospaces synchronise over the substrate (R5).
//! - [`manager`] — the *Platform Manager* model: a [`View`](manager::View) of
//!   the operator's holospaces and the Intent surface (provision · open a
//!   lifecycle session · synchronise). A rendered console is a thin
//!   presentation over this model.
//! - [`wasm`] — WebAssembly module validation + the substrate's closed host
//!   surface. Conformance: `CC-5`.
//!
//! The substrate seams holospaces drives — `KappaStore`, `KappaSync`,
//! `ContainerRuntime`, the `.holo` executor — are defined in hologram and
//! re-exported here for convenience under [`substrate`]. Conformance: `CC-3`
//! (peer storage). The *Platform Manager* (the Hologram platform) is itself a
//! holospace, hosted on this repo's GitHub Pages as content (not a host).
//!
//! ## The laws
//!
//! Every part upholds the laws (arc42 chapter 2,
//! `docs/src/arc42/adoc/02_architecture_constraints.adoc`): **L1** identity is
//! content, not location; **L2** operate only on canonical forms; **L3** the
//! store is the memory, RAM is a cache; **L4** everything goes through the
//! substrate; **L5** verify by re-derivation.
//!
//! ## Quality commitments
//!
//! Quality commitments (arc42 chapter 10) are enforced from the beginning by
//! the workspace lints (`unsafe_code = "forbid"`, `missing_docs`, clippy) and
//! by CI (`fmt`, `clippy -D warnings`, `doc`, the unit / integration / e2e
//! test tiers, and the V&V). Components are validated against external
//! authorities (`CC-*`), never against themselves.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub mod boot;
/// The `.holo` Engine (the host-side execution backend; `std` only — the CPU
/// kernels' float math needs `std`).
#[cfg(feature = "std")]
pub mod engine;
pub mod identity;
pub mod manager;
pub mod peer;
pub mod realizations;
pub mod surface;
/// Wasm module validation (the host-side provisioning surface; `std` only).
#[cfg(feature = "std")]
pub mod wasm;

/// The substrate contracts holospaces drives, re-exported from
/// [hologram](https://github.com/Hologram-Technologies/hologram)
/// (`hologram_substrate_core`) — defined there, consumed here by reference
/// (ADR-006). `CC-3` validates a peer's store against hologram's conformance
/// battery; `CC-2` validates the `.holo` engine against the native executor.
pub mod substrate {
    pub use hologram_substrate_core::{
        get_with_fetch, Capabilities, ContainerHandle, ContainerInfo, ContainerRuntime,
        ContainerState, GarbageCollect, KappaStore, KappaSync, Realization,
    };
}

pub use realizations::{address, verify, Axis, Holospace, Kappa, Source};
pub use substrate::Capabilities;
