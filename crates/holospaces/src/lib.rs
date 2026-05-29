//! # holospaces
//!
//! UOR-native boot layer over the [hologram](https://github.com/Hologram-Technologies/hologram)
//! substrate. holospaces provisions and runs *holospaces* — bootable,
//! content-addressed environments — and ships the Hologram Platform Manager.
//!
//! This crate is the workspace anchor. The building blocks defined by the
//! architecture (arc42 chapter 5 in `docs/`) — Realizations, the Boot Layer,
//! the `.holo` Engine, Identity, and the Platform Manager — are added as the
//! workspace grows; each lands with its conformance witness (`vv/`, a `CC-*`
//! row of the Conformance catalog). The documentation in `docs/` is
//! authoritative; this code traces back to it.
//!
//! Quality commitments (arc42 chapter 10) are enforced from the beginning by
//! the workspace lints (`unsafe_code = "forbid"`, `missing_docs`, clippy) and
//! by CI (`fmt`, `clippy -D warnings`, the test tiers, and the V&V).
