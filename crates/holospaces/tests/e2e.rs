//! End-to-end tests (the *e2e* tier).
//!
//! E2E tests exercise whole operator flows — provisioning and booting a
//! holospace, its lifecycle, and the Hologram Platform Manager. Native flows
//! run here; browser flows (the Manager on GitHub Pages) are added via a
//! browser runner when the browser peer lands. See arc42 chapter 10
//! (Verification and Validation). CI runs this tier via
//! `cargo test --workspace --test e2e`.
