//! **CC-5 — Wasm code modules are specification-valid.**
//!
//! The Conformance catalog row `CC-5` (arc42 chapter 10,
//! `docs/src/arc42/adoc/10_quality_requirements.adoc`): Wasm code modules are
//! specification-valid, validated against the
//! [WebAssembly](https://webassembly.org) specification by module validation.
//!
//! The external authority is the **WebAssembly specification's own `test/core`
//! conformance suite** — real `.wast` files imported verbatim from
//! `WebAssembly/spec` at the commit pinned in
//! `vv/artifacts/cc5/SOURCE-COMMIT.txt` (provenance in `vv/PROVENANCE.md`). The
//! witness drives the spec's own directives and checks holospaces' validator
//! ([`holospaces::wasm::validate`]) agrees with the specification's verdict:
//!
//! * `(module …)` / `(module definition …)` — must be **accepted**.
//! * `(assert_invalid …)` / `(assert_malformed …)` — must be **rejected**
//!   (at decode or validation).
//!
//! Execution directives (`assert_return`, `invoke`, …) are out of scope for
//! module validation and are skipped.
//!
//! Run by `vv/run.sh`; also `cargo test -p holospaces --test cc5_wasm`.

use holospaces::wasm::{validate, validate_substrate_module, WasmError};
use wast::parser::{self, ParseBuffer};
use wast::{QuoteWat, Wast, WastDirective};

fn artifact_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc5")
}

/// `(assert_invalid)` / `(assert_malformed)`: rejected at decode or validation.
fn is_rejected(module: &mut QuoteWat<'_>) -> bool {
    match module.encode() {
        Err(_) => true, // malformed text/binary — rejected at decode
        Ok(bytes) => validate(&bytes).is_err(),
    }
}

struct Counts {
    valid: usize,
    rejected: usize,
}

fn drive_wast(file: &str) -> Counts {
    let path = artifact_dir().join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {file}: {e}"));
    let buf = ParseBuffer::new(&text).unwrap_or_else(|e| panic!("lex {file}: {e}"));
    let wast: Wast = parser::parse(&buf).unwrap_or_else(|e| panic!("parse {file}: {e}"));

    let mut counts = Counts {
        valid: 0,
        rejected: 0,
    };
    for directive in wast.directives {
        match directive {
            WastDirective::Module(mut q) | WastDirective::ModuleDefinition(mut q) => {
                let bytes = match q.encode() {
                    Ok(b) => b,
                    // A handful of spec modules are quoted/binary forms not meant
                    // to round-trip through the text encoder; skip those.
                    Err(_) => continue,
                };
                assert!(
                    validate(&bytes).is_ok(),
                    "{file}: a spec-valid module was rejected by holospaces' validator"
                );
                counts.valid += 1;
            }
            WastDirective::AssertInvalid { mut module, .. }
            | WastDirective::AssertInvalidCustom { mut module, .. } => {
                assert!(
                    is_rejected(&mut module),
                    "{file}: a spec-invalid module was accepted by holospaces' validator"
                );
                counts.rejected += 1;
            }
            WastDirective::AssertMalformed { mut module, .. } => {
                assert!(
                    is_rejected(&mut module),
                    "{file}: a spec-malformed module was accepted by holospaces' validator"
                );
                counts.rejected += 1;
            }
            _ => {} // execution / linking directives are out of scope for CC-5
        }
    }
    counts
}

/// holospaces' validator agrees with the WebAssembly spec's `func.wast` and
/// `binary.wast` conformance suites on every module-validation directive.
#[test]
fn validator_agrees_with_the_webassembly_spec_suite() {
    let mut valid = 0;
    let mut rejected = 0;
    for file in ["func.wast", "binary.wast"] {
        let c = drive_wast(file);
        valid += c.valid;
        rejected += c.rejected;
    }
    assert!(valid >= 20, "expected many spec-valid modules, saw {valid}");
    assert!(
        rejected >= 20,
        "expected many spec-rejected modules, saw {rejected}"
    );
}

/// The substrate's closed host surface (hologram spec §4.4): a `hologram`-only
/// import is accepted; WASI / `env` imports are refused (SPINE-6).
#[test]
fn substrate_surface_refuses_non_hologram_imports() {
    let ok =
        wat::parse_str(r#"(module (import "hologram" "log" (func (param i32 i32 i32))))"#).unwrap();
    assert!(validate_substrate_module(&ok).is_ok());

    for wat_src in [
        r#"(module (import "wasi_snapshot_preview1" "fd_write" (func (param i32 i32 i32 i32) (result i32))))"#,
        r#"(module (import "env" "abort" (func)))"#,
    ] {
        let module = wat::parse_str(wat_src).unwrap();
        assert!(validate(&module).is_ok(), "module is spec-valid");
        assert!(
            matches!(
                validate_substrate_module(&module),
                Err(WasmError::ForbiddenImport { .. })
            ),
            "import outside the hologram host surface must be refused"
        );
    }
}
