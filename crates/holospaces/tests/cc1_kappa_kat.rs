//! **CC-1 — κ-labels are correct content addresses.**
//!
//! The Conformance catalog row `CC-1` (arc42 chapter 10,
//! `docs/src/arc42/adoc/10_quality_requirements.adoc`): holospaces' κ-labels
//! are correct content addresses under the σ-axis hash standards (BLAKE3;
//! FIPS 180-4 SHA-2; FIPS 202 SHA-3; Keccak), enforced by re-derivation
//! against imported known-answer test vectors (Law L5).
//!
//! holospaces mints κ-labels through the hologram substrate
//! ([`holospaces::realizations::address_on`] → `hologram_substrate_core`'s
//! σ-axis). This witness validates against an *external* authority — the
//! published KAT vectors imported in `vv/artifacts/cc1/hash-kats.json`
//! (provenance in `vv/PROVENANCE.md`), never against holospaces itself:
//!
//! 1. **κ-labels are the published hashes.** Because the substrate addresses
//!    raw bytes directly, holospaces' κ-label digest for each KAT message
//!    equals the published digest from the standard — a direct re-derivation
//!    against the imported vectors (Law L5).
//! 2. **The oracle is independently the standard.** The reference hash crates
//!    (`blake3`, `sha2`, `sha3`) reproduce the same published digests,
//!    cross-checking the vectors against a second implementation of the
//!    standard.
//!
//! Run by `vv/run.sh`; also `cargo test -p holospaces --test cc1_kappa_kat`.

use holospaces::realizations::{address_on, verify, Axis};
use serde_json::Value;
use sha2::Digest as _;

fn kat_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vv/artifacts/cc1/hash-kats.json")
}

fn decode_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "hex must have even length");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex byte"))
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn axis_of(token: &str) -> Axis {
    match token {
        "blake3" => Axis::Blake3,
        "sha256" => Axis::Sha256,
        "sha3-256" => Axis::Sha3_256,
        "keccak256" => Axis::Keccak256,
        "sha512" => Axis::Sha512,
        other => panic!("KAT names an unsupported axis: {other}"),
    }
}

/// The hologram-minted κ-label digest for `bytes` on `axis`: parse the
/// `<axis>:<hex>` wire label holospaces produces and return the hex digest.
fn kappa_digest(axis: Axis, bytes: &[u8]) -> String {
    let label = address_on(bytes, axis).expect("substrate addresses the bytes");
    let wire = String::from_utf8(label).expect("κ-label is ASCII");
    let (got_axis, hex) = wire.split_once(':').expect("κ-label is <axis>:<hex>");
    assert_eq!(got_axis, axis.token());
    hex.to_owned()
}

/// The reference hash for an axis — the standard implementation, cross-checked
/// against the published KATs in the second test.
fn reference_hash(axis: &str, bytes: &[u8]) -> String {
    match axis {
        "sha256" => to_hex(&sha2::Sha256::digest(bytes)),
        "sha3-256" => to_hex(&sha3::Sha3_256::digest(bytes)),
        "keccak256" => to_hex(&sha3::Keccak256::digest(bytes)),
        "blake3" => blake3::hash(bytes).to_hex().to_string(),
        other => panic!("unknown σ-axis: {other}"),
    }
}

fn load_kats() -> Value {
    let raw =
        std::fs::read(kat_path()).unwrap_or_else(|e| panic!("read {}: {e}", kat_path().display()));
    serde_json::from_slice(&raw).expect("hash-kats.json is valid JSON")
}

/// Layer 1: holospaces' κ-label digests ARE the published hash-standard digests
/// for the KAT messages — a direct re-derivation against the imported vectors.
#[test]
fn holospaces_kappa_digests_equal_published_vectors() {
    let kats = load_kats();
    let axes = kats["axes"].as_object().expect("axes object");
    let mut checked = 0;
    for (axis_token, entry) in axes {
        let axis = axis_of(axis_token);
        for vector in entry["vectors"].as_array().expect("vectors array") {
            let msg = decode_hex(vector["msg_hex"].as_str().unwrap());
            let expected = vector["digest_hex"].as_str().unwrap();
            assert_eq!(
                kappa_digest(axis, &msg),
                expected,
                "holospaces {axis_token} κ-digest disagrees with the published KAT (msg_hex={})",
                vector["msg_hex"].as_str().unwrap()
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 8,
        "expected the full KAT battery, checked {checked}"
    );
}

/// Layer 2: the reference hash crates independently reproduce the published
/// vectors, cross-checking that the imported KATs are the standard's digests.
#[test]
fn reference_hashes_reproduce_published_vectors() {
    let kats = load_kats();
    for (axis, entry) in kats["axes"].as_object().unwrap() {
        for vector in entry["vectors"].as_array().unwrap() {
            let msg = decode_hex(vector["msg_hex"].as_str().unwrap());
            assert_eq!(
                reference_hash(axis, &msg),
                vector["digest_hex"].as_str().unwrap(),
                "reference {axis} disagrees with the published KAT"
            );
        }
    }
}

/// Re-derivation rejects content that does not match its claimed κ (Law L5).
#[test]
fn re_derivation_rejects_mismatched_content() {
    let k = holospaces::address(b"alpha");
    assert!(verify(b"alpha", &k).unwrap());
    assert!(!verify(b"beta", &k).unwrap());
}
