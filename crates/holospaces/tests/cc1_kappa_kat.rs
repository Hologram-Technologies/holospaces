//! **CC-1 — κ-labels are correct content addresses.**
//!
//! The Conformance catalog row `CC-1` (arc42 chapter 10,
//! `docs/src/arc42/adoc/10_quality_requirements.adoc`): holospaces' κ-labels
//! are correct content addresses under the σ-axis hash standards (BLAKE3;
//! FIPS 180-4 SHA-2; FIPS 202 SHA-3; Keccak), enforced by re-derivation
//! (Law L5).
//!
//! holospaces mints κ-labels through the hologram substrate
//! ([`holospaces::realizations::address`] / [`address_on`] →
//! `hologram_substrate_core`'s σ-axis). This witness mirrors hologram's own
//! `AS` conformance class (`as1_sigma_axis_equals_blake3_reference`): it
//! validates the σ-axis **byte-for-byte against the authoritative reference
//! implementations** of each standard — the `blake3`, `sha2`, and `sha3`
//! crates (the canonical implementations of BLAKE3 / FIPS 180-4 / FIPS 202,
//! pinned in `Cargo.lock`) — never against holospaces itself. No hand-authored
//! vectors: the external reference implementation is the oracle.
//!
//! Run by `vv/run.sh`; also `cargo test -p holospaces --test cc1_kappa_kat`.

use holospaces::realizations::{address, address_on, verify, Axis};
use sha2::Digest as _;

/// A deterministic spread of inputs: the standard short messages plus
/// byte-pattern inputs at sizes that cross BLAKE3's chunk/subtree boundaries
/// (1 KiB chunk; 2-chunk subtrees), exercising the streaming merge (AS-5).
fn inputs() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = vec![
        b"".to_vec(),
        b"abc".to_vec(),
        b"The quick brown fox jumps over the lazy dog".to_vec(),
    ];
    for len in [1usize, 63, 64, 1024, 1025, 4096, 10_000] {
        v.push((0..len).map(|i| (i % 251) as u8).collect());
    }
    v
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The authoritative reference digest for an axis — the canonical crate
/// implementing that published standard.
fn reference_digest(axis: Axis, bytes: &[u8]) -> String {
    match axis {
        Axis::Blake3 => blake3::hash(bytes).to_hex().to_string(),
        Axis::Sha256 => to_hex(&sha2::Sha256::digest(bytes)),
        Axis::Sha512 => to_hex(&sha2::Sha512::digest(bytes)),
        Axis::Sha3_256 => to_hex(&sha3::Sha3_256::digest(bytes)),
        Axis::Keccak256 => to_hex(&sha3::Keccak256::digest(bytes)),
    }
}

const AXES: [Axis; 5] = [
    Axis::Blake3,
    Axis::Sha256,
    Axis::Sha3_256,
    Axis::Keccak256,
    Axis::Sha512,
];

/// AS-1: holospaces' κ-label digest equals the reference implementation's
/// digest, byte-for-byte, on every σ-axis and across chunk/subtree boundaries.
#[test]
fn kappa_digest_equals_reference_implementation() {
    for input in inputs() {
        for axis in AXES {
            let wire = String::from_utf8(address_on(&input, axis).expect("address")).unwrap();
            let (got_axis, hex) = wire.split_once(':').expect("κ-label is <axis>:<hex>");
            assert_eq!(got_axis, axis.token());
            assert_eq!(
                hex,
                reference_digest(axis, &input),
                "{axis} κ-digest disagrees with the reference implementation for a {}-byte input",
                input.len()
            );
        }
    }
}

/// AS-2: the default axis is blake3 and the wire form is `blake3:<64 hex>`.
#[test]
fn default_axis_is_canonical_blake3_label() {
    let k = address(b"holospace");
    assert_eq!(k.sigma_axis(), Some("blake3"));
    let hex = k.as_str().split_once(':').unwrap().1;
    assert_eq!(hex.len(), 64);
    assert!(hex
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    assert_eq!(hex, reference_digest(Axis::Blake3, b"holospace"));
}

/// AS-3: equal content ⇒ equal κ (determinism).
#[test]
fn addressing_is_deterministic() {
    for input in inputs() {
        assert_eq!(address(&input), address(&input));
    }
}

/// AS-4: a single-bit change changes the κ-label (collision-sensitivity).
#[test]
fn single_bit_change_changes_the_label() {
    let base = vec![0u8; 64];
    let k0 = address(&base);
    for bit in [0usize, 7, 200, 511] {
        let mut flipped = base.clone();
        flipped[bit / 8] ^= 1 << (bit % 8);
        assert_ne!(address(&flipped), k0, "flipping bit {bit} must change κ");
    }
}

/// Re-derivation rejects content that does not match its claimed κ (Law L5).
#[test]
fn re_derivation_rejects_mismatched_content() {
    let k = address(b"alpha");
    assert!(verify(b"alpha", &k).unwrap());
    assert!(!verify(b"beta", &k).unwrap());
}
