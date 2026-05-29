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
//! - [`boot`] — the environment-agnostic core: ingest, resolve (the
//!   substrate's verify-on-receipt read), and a [`Session`](boot::Session) that
//!   drives the lifecycle through hologram's `ContainerRuntime`.
//! - [`identity`] — self-sovereign sign-in and the keying that links an
//!   operator's instances.
//!
//! The substrate seams holospaces drives — `KappaStore`, `KappaSync`,
//! `ContainerRuntime`, the `.holo` executor — are defined in hologram and
//! re-exported here for convenience under [`substrate`]. The *Platform
//! Manager* (the Hologram platform — the operator console) is itself a
//! holospace, managed through these types and hosted on this repo's GitHub
//! Pages as content (not a host); it is not part of this library.
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

pub mod boot;
pub mod identity;
pub mod realizations;
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
