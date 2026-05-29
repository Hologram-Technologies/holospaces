//! **Wasm execution path** — validating Wasm code modules.
//!
//! Realizes the cross-cutting concept *Two compute forms* (arc42 chapter 8,
//! `docs/src/arc42/adoc/08_concepts.adoc`): general/system code is a *Wasm code
//! module*, run by [hologram](https://github.com/Hologram-Technologies/hologram)'s
//! runtime via its `ContainerEngine` seam.
//!
//! A Wasm code module is accepted only if it is *specification-valid* (the
//! [WebAssembly](https://webassembly.org) specification) — this is the κ
//! boundary's structural guarantee for the Wasm form. `validate` checks
//! spec-validity; `validate_substrate_module` additionally enforces the
//! substrate's closed host surface (spec §4.4): a substrate container may
//! import only from the single `hologram` host module — WASI / `env` and other
//! ambient imports are refused (SPINE-6), so the workload bound is structural.
//!
//! Conformance: `CC-5` (witnessed against the WebAssembly specification, see
//! `vv/`).

use core::fmt;

use wasmparser::{Parser, Payload, Validator};

/// The single host import module a substrate Wasm container may use
/// (WebAssembly host surface, hologram spec §4.4).
pub const SUBSTRATE_HOST_MODULE: &str = "hologram";

/// Validate that `module` is a specification-valid WebAssembly module.
///
/// # Errors
///
/// [`WasmError::Invalid`] if the bytes are not a valid Wasm module per the
/// WebAssembly specification.
pub fn validate(module: &[u8]) -> Result<(), WasmError> {
    Validator::new()
        .validate_all(module)
        .map(|_| ())
        .map_err(|e| WasmError::Invalid(e.to_string()))
}

/// Validate a Wasm module for the substrate: spec-valid **and** importing only
/// from the `hologram` host module (the closed execution surface, spec §4.4).
///
/// # Errors
///
/// [`WasmError::Invalid`] if not spec-valid; [`WasmError::ForbiddenImport`] if
/// it declares an import outside the `hologram` host surface.
pub fn validate_substrate_module(module: &[u8]) -> Result<(), WasmError> {
    validate(module)?;
    for payload in Parser::new(0).parse_all(module) {
        let payload = payload.map_err(|e| WasmError::Invalid(e.to_string()))?;
        if let Payload::ImportSection(reader) = payload {
            for import in reader {
                let import = import.map_err(|e| WasmError::Invalid(e.to_string()))?;
                if import.module != SUBSTRATE_HOST_MODULE {
                    return Err(WasmError::ForbiddenImport {
                        module: import.module.to_owned(),
                        name: import.name.to_owned(),
                    });
                }
            }
        }
    }
    Ok(())
}

/// Why a Wasm code module was rejected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WasmError {
    /// The module is not specification-valid.
    Invalid(String),
    /// The module imports outside the substrate's closed host surface.
    ForbiddenImport {
        /// The import's module namespace.
        module: String,
        /// The import's field name.
        name: String,
    },
}

impl fmt::Display for WasmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WasmError::Invalid(e) => write!(f, "Wasm module is not specification-valid: {e}"),
            WasmError::ForbiddenImport { module, name } => write!(
                f,
                "Wasm module imports '{module}::{name}' outside the '{SUBSTRATE_HOST_MODULE}' host surface"
            ),
        }
    }
}

impl std::error::Error for WasmError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_module_is_spec_valid() {
        // The 8-byte module preamble (`\0asm` + version 1) is a valid module.
        let module = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
        assert!(validate(&module).is_ok());
    }

    #[test]
    fn bad_magic_is_rejected() {
        let module = [0x00, 0x61, 0x73, 0x6e, 0x01, 0x00, 0x00, 0x00];
        assert!(matches!(validate(&module), Err(WasmError::Invalid(_))));
    }
}
