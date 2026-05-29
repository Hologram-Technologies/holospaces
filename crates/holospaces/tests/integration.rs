//! Integration tests (the *integration* tier).
//!
//! Integration tests exercise a building block's public surface and the
//! composition of building blocks (arc42 chapter 5). They are added as those
//! building blocks land, each alongside its conformance witness (`vv/`,
//! `CC-*`). CI runs this tier via `cargo test --workspace --test integration`.
