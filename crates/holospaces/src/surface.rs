//! **Execution Surface** — the Wasm-native Linux/POSIX surface.
//!
//! Realizes the *Execution Surface* building block (arc42 chapter 5,
//! `docs/src/arc42/adoc/05_building_block_view.adoc`) and the cross-cutting
//! concept *The execution surface* (arc42 chapter 8,
//! `docs/src/arc42/adoc/08_concepts.adoc`). It is the resolution of the open
//! decision RT1 (arc42 chapter 11) as ADR-008 (chapter 9).
//!
//! A holospace's general/system code — the second of the two compute forms — is
//! a *Wasm-recompiled userland*: a κ-addressed Wasm code module that imports
//! only the substrate's host ABI (the `hologram` host module — the syscall
//! boundary) and presents the container ABI hologram's `ContainerRuntime`
//! drives. holospaces *defines, enforces, and boots* this surface: it validates
//! a userland against the host-ABI contract ([`validate_userland`]), composes a
//! bootable [`Holospace`] ([`compose`]), and the substrate runtime spawns it
//! over the peer's `ContainerEngine` — Wasmtime natively, hologram's `wasmi`
//! interpreter (`hologram-runtime-bare`) in the browser and on bare-metal, where
//! a JIT cannot run. A userland is κ-addressed *content* the platform hosts.
//!
//! Because a userland is content (a κ), not a located OCI image, code identity
//! stays content (Law L1), dedups and verifies like any κ (Laws L2, L5), and
//! the same userland κ boots on any peer (Q6) — all without a second execution
//! medium (Law L4). Conformance: `CC-6` (arc42 chapter 10), witnessed on both
//! the native and interpreter engines.

use hologram_substrate_core::Capabilities;

use crate::realizations::{Holospace, Kappa, Source};

/// The single host-import namespace a recompiled userland may bind — the
/// substrate's host ABI, the syscall boundary (the closed host surface of
/// `CC-5`). Identical to [`crate::wasm::SUBSTRATE_HOST_MODULE`]; the hologram
/// host ABI is defined upstream and consumed by reference (ADR-006).
pub const HOST_NAMESPACE: &str = "hologram";

/// The container ABI entry points a recompiled userland exports for hologram's
/// `ContainerRuntime` to drive its lifecycle. The contract is hologram's,
/// consumed by reference (ADR-006); the surface validator only checks that a
/// userland presents it.
pub const CONTAINER_ABI: [&str; 5] = [
    "hg_init",
    "hg_event",
    "hg_suspend",
    "hg_resume",
    "hg_callback",
];

/// Compose a [`Holospace`] from a recompiled-userland entry module κ and a
/// capability set — the execution-surface provisioning path (ADR-008).
///
/// The entry module is the Container ID's code the runtime spawns; the
/// holospace identity is reproducible from it (`Source::Userland`), so the same
/// userland κ yields the same holospace on any peer (Q6, QS1).
#[must_use]
pub fn compose(entry: Kappa, capabilities: Capabilities) -> Holospace {
    Holospace::compose(Source::Userland { entry }, capabilities)
}

/// Validate that `module` is a recompiled userland fit for the execution
/// surface: specification-valid WebAssembly that imports *only* the substrate
/// host ABI ([`HOST_NAMESPACE`]) and *presents the full container ABI*
/// ([`CONTAINER_ABI`]). This is the κ-boundary contract the surface enforces
/// before a userland may be a holospace's code (ADR-008; `CC-6`).
///
/// The host-ABI import bound is the substrate's closed host surface check
/// ([`crate::wasm::validate_substrate_module`], the WebAssembly specification
/// §4.4); the container-ABI export check confirms the runtime can drive it.
///
/// # Errors
///
/// [`SurfaceError::Wasm`] if the module is not spec-valid or imports outside the
/// host ABI; [`SurfaceError::MissingAbiExport`] if it does not present the full
/// container ABI.
#[cfg(feature = "std")]
pub fn validate_userland(module: &[u8]) -> Result<(), SurfaceError> {
    use wasmparser::{ExternalKind, Parser, Payload};

    crate::wasm::validate_substrate_module(module).map_err(SurfaceError::Wasm)?;

    let mut exported = [false; CONTAINER_ABI.len()];
    for payload in Parser::new(0).parse_all(module) {
        let payload = payload
            .map_err(|e| SurfaceError::Wasm(crate::wasm::WasmError::Invalid(e.to_string())))?;
        if let Payload::ExportSection(reader) = payload {
            for export in reader {
                let export = export.map_err(|e| {
                    SurfaceError::Wasm(crate::wasm::WasmError::Invalid(e.to_string()))
                })?;
                if export.kind == ExternalKind::Func {
                    if let Some(i) = CONTAINER_ABI.iter().position(|n| *n == export.name) {
                        exported[i] = true;
                    }
                }
            }
        }
    }
    if let Some(i) = exported.iter().position(|present| !present) {
        return Err(SurfaceError::MissingAbiExport(CONTAINER_ABI[i]));
    }
    Ok(())
}

/// Why a module is not a valid recompiled userland for the execution surface.
#[cfg(feature = "std")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceError {
    /// The module is not spec-valid, or imports outside the host ABI.
    Wasm(crate::wasm::WasmError),
    /// The module does not export a required container-ABI entry point.
    MissingAbiExport(&'static str),
}

#[cfg(feature = "std")]
impl core::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SurfaceError::Wasm(e) => write!(f, "userland is not surface-valid: {e}"),
            SurfaceError::MissingAbiExport(n) => {
                write!(f, "userland does not present container-ABI export '{n}'")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SurfaceError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realizations::address;

    fn caps() -> Capabilities {
        Capabilities {
            storage_roots: alloc::vec::Vec::new(),
            storage_quota_bytes: 0,
            network_fetch: false,
            network_announce: false,
            publish_channels: alloc::vec::Vec::new(),
            subscribe_channels: alloc::vec::Vec::new(),
            memory_max_bytes: 0,
            cpu_time_per_event_ms: 0,
            priority_weight: 0,
        }
    }

    #[test]
    fn compose_yields_a_userland_holospace() {
        let entry = address(b"a recompiled userland entry module");
        let hs = compose(entry, caps());
        assert_eq!(hs.source(), &Source::Userland { entry });
        // The entry module is the Container ID's code (the runtime spawns it).
        assert_eq!(hs.container_manifest().code, entry);
    }

    #[cfg(feature = "std")]
    #[test]
    fn host_namespace_matches_the_substrate_host_surface() {
        assert_eq!(HOST_NAMESPACE, crate::wasm::SUBSTRATE_HOST_MODULE);
    }
}
