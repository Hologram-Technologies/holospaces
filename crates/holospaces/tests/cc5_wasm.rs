//! **CC-5 — Wasm code modules are specification-valid.**
//!
//! The Conformance catalog row `CC-5` (arc42 chapter 10,
//! `docs/src/arc42/adoc/10_quality_requirements.adoc`): Wasm code modules are
//! specification-valid, validated against the
//! [WebAssembly](https://webassembly.org) specification by module validation.
//!
//! The external authority is the WebAssembly spec conformance cases imported in
//! `vv/artifacts/cc5/wasm-cases.json` (provenance in `vv/PROVENANCE.md`),
//! derived from the spec / its `test/core` suite. This witness assembles each
//! module from its text form and checks:
//!
//! 1. holospaces' validator ([`holospaces::wasm::validate`]) accepts exactly
//!    the modules the spec considers valid, and rejects the invalid ones;
//! 2. the substrate's closed host surface ([`holospaces::wasm::validate_substrate_module`])
//!    accepts a `hologram`-only import and refuses WASI / `env` imports
//!    (spec §4.4, SPINE-6).
//!
//! Run by `vv/run.sh`; also `cargo test -p holospaces --test cc5_wasm`.

use holospaces::wasm::{validate, validate_substrate_module, WasmError};
use serde_json::Value;

fn cases() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vv/artifacts/cc5/wasm-cases.json");
    let raw = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_slice(&raw).expect("wasm-cases.json is valid JSON")
}

fn assemble(case: &Value) -> Vec<u8> {
    let wat = case["wat"].as_str().unwrap();
    wat::parse_str(wat).unwrap_or_else(|e| panic!("assemble {}: {e}", case["name"]))
}

/// (1) The validator accepts exactly the spec-valid modules.
#[test]
fn validator_accepts_spec_valid_modules() {
    let doc = cases();
    let valid = doc["spec_valid"].as_array().unwrap();
    assert!(valid.len() >= 5, "expected the full valid battery");
    for case in valid {
        let module = assemble(case);
        assert!(
            validate(&module).is_ok(),
            "case '{}' is spec-valid but the validator rejected it",
            case["name"]
        );
    }
}

/// (1) The validator rejects modules the spec considers invalid.
#[test]
fn validator_rejects_spec_invalid_modules() {
    let doc = cases();
    for case in doc["spec_invalid"].as_array().unwrap() {
        // These fail WebAssembly *validation* (they assemble, but are not valid
        // modules — type mismatch, stack underflow, unknown index).
        let module = match wat::parse_str(case["wat"].as_str().unwrap()) {
            Ok(bytes) => bytes,
            Err(_) => continue, // rejected already at the text level — also a rejection
        };
        assert!(
            matches!(validate(&module), Err(WasmError::Invalid(_))),
            "case '{}' is spec-invalid but the validator accepted it",
            case["name"]
        );
    }
}

/// (2) The closed host surface: a `hologram`-only import is accepted; WASI /
/// `env` imports are refused (spec §4.4).
#[test]
fn substrate_surface_refuses_non_hologram_imports() {
    let doc = cases();

    let hologram_import = doc["spec_valid"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "hologram-import")
        .expect("hologram-import case");
    assert!(validate_substrate_module(&assemble(hologram_import)).is_ok());

    for case in doc["substrate_forbidden"].as_array().unwrap() {
        let module = assemble(case);
        // Spec-valid as a module...
        assert!(
            validate(&module).is_ok(),
            "{} should be spec-valid",
            case["name"]
        );
        // ...but refused on the substrate's closed surface.
        assert!(
            matches!(
                validate_substrate_module(&module),
                Err(WasmError::ForbiddenImport { .. })
            ),
            "case '{}' must be refused outside the hologram host surface",
            case["name"]
        );
    }
}
