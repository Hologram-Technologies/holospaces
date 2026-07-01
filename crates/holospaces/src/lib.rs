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
//! - [`surface`] — the *Execution Surface*: the κ-addressed Wasm code-module
//!   form (the second compute form) and the host-ABI contract it must bind
//!   (ADR-008, generalized by ADR-009). Conformance: `CC-6`.
//! - [`disk`] — the *κ-disk*: a [`KappaStore`](substrate::KappaStore)-backed
//!   `BlockDevice` (hologram's HAL), an operating-system image as κ-addressed
//!   content the [execution surface](surface) reads (ADR-009). Conformance:
//!   `CC-7`.
//! - [`emulator`] — the *system emulator* core, with **two ISA targets**
//!   (ADR-021) over a shared substrate-backed device bus (the κ-disk/9p/NAT
//!   `virtio` servicing, used by both ISAs with no per-ISA re-implementation): a real
//!   **RISC-V** machine (RV64GC = IMAFDC + Zicsr, machine/supervisor traps,
//!   Sv39/Sv48/Sv57 paging, CLINT interrupts, SBI) verified against the official
//!   RISC-V conformance suite (`CC-9`); and a real **AArch64** machine
//!   ([`aarch64`](emulator::aarch64) — the A64 ISA + the EL0/EL1 exception model,
//!   VMSAv8-64 paging, the ARM `virt` platform: GICv2, the generic timer, PSCI, a
//!   PL011 console) that boots a real `arm64` Linux to userspace, verified against
//!   `qemu-system-aarch64` (`CC-35`/`CC-36`/`CC-37`). The emulator codemodule
//!   wraps either core to boot an arbitrary OS image on its selected
//!   architecture (ADR-009).
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
//! - [`projection`] — the *Workspace Projection* model: a view + intent over a
//!   *running* holospace — an Editor/FS view that reads the environment's
//!   content by κ, and a [`Terminal`](projection::Workspace) that publishes the
//!   operator's input as canonical events ([`Intent`](projection::Intent))
//!   advancing the holospace's κ snapshot. The Codespaces/Gitpod experience
//!   (ADR-009). Conformance: `CC-11`.
//! - [`wasm`] — WebAssembly module validation + the substrate's closed host
//!   surface. Conformance: `CC-5`.
//! - [`oci`] — OCI image ingestion: a devcontainer's operating-system image is
//!   ingested at the boundary into κ-addressed content, each blob verified by
//!   re-derivation against its OCI `sha256` digest (an OCI digest is a κ-label;
//!   Law L5). Conformance: `CC-10`.
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

/// Layer Assembler: OCI image layers → a bootable `ext4` root filesystem
/// (the *Rootfs Assembly* of SD5; arc42 ch.5). Connects `CC-10` → `CC-7`.
/// no_std + alloc — part of the portable peer core, like the κ-disk it feeds.
pub mod assembly;
pub mod boot;
/// Docker Compose service resolution for a `dockerComposeFile` Dev Container
/// (`CC-27`); a host-side provisioning surface, `std` only.
#[cfg(feature = "std")]
pub mod compose;
/// The control plane's content-addressed reconfiguration of a running holospace
/// — lifecycle / storage / network / account directives published over the
/// substrate and applied by the instance (ADR-018; `CC-28`).
pub mod config;
/// The uor-native content network (`CC-38`): the substrate's `BareNetSync` over a
/// portable [`NetworkInterface`](hologram_bare_hal::NetworkInterface), so a
/// browser peer and a bare-metal peer fetch each other's content the same
/// content-addressed way (the "browser as a router" model). Part of the portable
/// peer core (no_std + alloc).
pub mod content_net;
pub mod disk;
/// Dockerfile parsing + substrate-native build of a `build.dockerfile` Dev
/// Container (`CC-26`); a host-side provisioning surface, `std` only.
#[cfg(feature = "std")]
pub mod dockerfile;
pub mod emulator;
/// The `.holo` Engine (the host-side execution backend; `std` only — the CPU
/// kernels' float math needs `std`).
#[cfg(feature = "std")]
pub mod engine;
pub mod identity;
/// The internet import boundary (ADR-013; `CC-20`): fetch a repository by URL and
/// pull its devcontainer's OCI image from a registry, verified by re-derivation.
/// Host-only (`net` feature) — links an HTTP(S) client.
#[cfg(feature = "net")]
pub mod import;
/// Boot Orchestrator (arc42 ch.5): generates the machine's device tree and boots
/// a kernel + κ-disk on the emulator — the first-class `CC-14` boot operation.
pub mod machine;
pub mod manager;
/// Generate the PID-1 init that runs an arbitrary OCI image's real entrypoint (`Entrypoint`/`Cmd`/
/// `Env`/`WorkingDir`/`User`) — the keystone of "run any docker image". Conformance: `CC-65`. Host-side
/// tooling (the `holo run` pipeline produces a bootable `.holo`; the browser resumes the result), so it
/// is not built for `wasm32` (and a rustc 1.95 lint ICE on that target is thereby avoided).
#[cfg(not(target_arch = "wasm32"))]
pub mod image_init;
/// OCI image ingestion (the host-side provisioning surface for a devcontainer's
/// operating-system image; `std` only). Conformance: `CC-10`.
#[cfg(feature = "std")]
pub mod oci;
pub mod peer;
pub mod personalization;
pub mod projection;
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

pub use emulator::Arch;
pub use realizations::{address, verify, Axis, Holospace, Kappa, Source};
pub use substrate::Capabilities;

/// The default Dev Container base image, used when a repository declares no
/// `devcontainer.json` (the Dev Container spec's fallback) — the Codespaces
/// model, where a repo with no config still opens into a *usable* environment.
///
/// `buildpack-deps:trixie-scm` is the official Debian-based image the language
/// runtime images build on: it ships the basic developer utilities an operator
/// expects on entry — `curl`, `wget`, `ca-certificates`, and the SCM clients
/// (`git`, …) — over a real `apt` userland, so the booted holospace is
/// immediately functional (not a bare base where "not even `curl` works"). It
/// is multi-arch and, crucially, publishes **both** `riscv64` **and** `arm64`
/// variants, so the same default boots on either emulator target (`CC-9`,
/// `CC-35`) — the selected manifest follows the operator's architecture (Law
/// L1, ADR-021). Heavier `apt install`s layer on top over the network (`CC-16`).
///
/// Defined at the crate root (always compiled, not only the `net` import
/// surface) so the browser peer can name the usable default when a repository
/// declares no devcontainer — a single source of truth across native and wasm.
pub const DEFAULT_DEVCONTAINER_IMAGE: &str = "buildpack-deps:trixie-scm";
