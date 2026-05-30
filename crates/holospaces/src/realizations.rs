//! **Realizations** — holospaces' canonical-form layer over the hologram substrate.
//!
//! Realizes the *Realizations* building block (arc42 chapter 5,
//! `docs/src/arc42/adoc/05_building_block_view.adoc`) and the cross-cutting
//! concept *Canonical forms and κ-labels* (arc42 chapter 8,
//! `docs/src/arc42/adoc/08_concepts.adoc`).
//!
//! Everything holospaces holds is a *canonical form* identified by a κ-label
//! ([`Kappa`] = [`hologram_substrate_core::KappaLabel71`], `<axis>:<hex>` =
//! `H(canonical_form)`). Identity is *what a thing is*, not *where* (Law L1);
//! holospaces holds κ-labels, not objects (Law L3).
//!
//! κ-addressing is the [hologram](https://github.com/Hologram-Technologies/hologram)
//! substrate's, consumed by reference (ADR-003, ADR-006): [`address`] /
//! [`verify`] wrap `hologram_substrate_core`'s σ-axis functions. holospaces
//! does not re-implement hashing or the canonical-form discipline.
//!
//! The chief holospaces canonical form is the [`Holospace`] — a hologram
//! [`Realization`]: IRI-tagged bytes that embed their operand κ-labels
//! (SPINE-2/3), composing a hologram [`ContainerManifest`] (the code) and
//! [`CapabilitySet`] (the authority).
//!
//! See the Conformance catalog row `CC-1` (arc42 chapter 10): κ-labels are
//! correct content addresses, witnessed against the published σ-axis hash test
//! vectors in `vv/`.

use core::fmt;

#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::{
    borrow::ToOwned,
    boxed::Box,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use hologram_realizations::{CapabilitySet, ContainerManifest};
use hologram_substrate_core::{
    address_bytes, address_bytes_axis, verify_kappa, AxisError, Capabilities, KappaLabel71,
    Realization, RealizationError, RealizationId, RefExtractor, References,
};

/// The realization registry holospaces resolves reachability (SPINE-3) with:
/// its own [`Holospace`] realization plus hologram's
/// (`hologram_realizations::REGISTRY` — `ContainerManifest`, `CapabilitySet`,
/// …). Used to walk a holospace's transitive closure when migrating it to
/// another peer.
#[must_use]
pub fn registry() -> Vec<(RealizationId, RefExtractor)> {
    let mut reg: Vec<(RealizationId, RefExtractor)> = vec![(
        Holospace::IRI,
        <Holospace as Realization>::references as RefExtractor,
    )];
    reg.extend_from_slice(hologram_realizations::REGISTRY);
    reg
}

/// A κ-label: a content address (`<axis>:<hex>`). The substrate's
/// [`KappaLabel71`] (blake3, 71 bytes) is holospaces' identity type — the same
/// type hologram's store, sync, and runtime speak, so holospaces adds no
/// parallel addressing.
pub type Kappa = KappaLabel71;

/// The κ-label of the empty canonical form — the conventional placeholder for
/// an absent operand (e.g. a manifest with no initial state).
#[must_use]
pub fn empty_kappa() -> Kappa {
    address_bytes(&[])
}

/// A σ-axis: the content-address hash family a κ-label is minted on. hologram's
/// own realizations are blake3 (the default); stored content keys are
/// axis-polymorphic. Each axis names the standard that is its external
/// authority for `CC-1` (see `vv/PROVENANCE.md`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    /// BLAKE3 (the BLAKE3 reference specification). The substrate default.
    Blake3,
    /// SHA-256 (NIST FIPS 180-4).
    Sha256,
    /// SHA3-256 (NIST FIPS 202).
    Sha3_256,
    /// Keccak-256 (the original Keccak permutation; pre-FIPS padding).
    Keccak256,
    /// SHA-512 (NIST FIPS 180-4).
    Sha512,
}

impl Axis {
    /// The wire token at the head of a κ-label minted on this axis.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Axis::Blake3 => "blake3",
            Axis::Sha256 => "sha256",
            Axis::Sha3_256 => "sha3-256",
            Axis::Keccak256 => "keccak256",
            Axis::Sha512 => "sha512",
        }
    }
}

impl fmt::Display for Axis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.token())
    }
}

/// Mint the κ-label of canonical bytes on the substrate's default axis (blake3),
/// via [`hologram_substrate_core::address_bytes`] (Law L2).
#[must_use]
pub fn address(canonical_bytes: &[u8]) -> Kappa {
    address_bytes(canonical_bytes)
}

/// Mint the κ-label of canonical bytes on a chosen σ-axis, returning the
/// variable-width on-the-wire `<axis>:<hex>` label bytes (Law L2).
///
/// # Errors
///
/// Returns [`AxisError`] if the axis is not wired in the substrate build.
pub fn address_on(canonical_bytes: &[u8], axis: Axis) -> Result<Vec<u8>, AxisError> {
    address_bytes_axis(axis.token(), canonical_bytes)
}

/// Verify canonical bytes against a claimed κ-label by re-derivation (Law L5),
/// via [`hologram_substrate_core::verify_kappa`].
///
/// Trust is in the math: bytes are accepted only if re-deriving their κ on the
/// label's axis reproduces it. This is what makes an untrusted gateway safe
/// (arc42 chapter 8, *Verify by re-derivation*).
///
/// # Errors
///
/// Returns [`AxisError`] if the label's axis is unsupported or malformed.
pub fn verify(canonical_bytes: &[u8], expected: &Kappa) -> Result<bool, AxisError> {
    verify_kappa(canonical_bytes, expected)
}

// ── the canonical-form encoding (hologram SPINE-2/3, mirrored for a holospaces
//    realization; the wire format hologram's `references()` dispatcher parses) ──

const KAPPA71: usize = 71;

/// Encode a realization's canonical form: `IRI\0` + `u32` ref-count + each
/// operand κ-label (71 bytes) + `u32` payload length + payload (SPINE-2).
pub(crate) fn encode(iri: &str, refs: &[Kappa], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(iri.len() + 1 + 8 + refs.len() * KAPPA71 + payload.len());
    out.extend_from_slice(iri.as_bytes());
    out.push(0);
    out.extend_from_slice(&(refs.len() as u32).to_le_bytes());
    for r in refs {
        out.extend_from_slice(r.as_array());
    }
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Inverse projection (SPINE-3): validate the leading IRI and recover exactly
/// the embedded operand κ-labels.
pub(crate) fn extract_refs(iri: &str, bytes: &[u8]) -> Result<References, RealizationError> {
    let nul = bytes
        .iter()
        .position(|&b| b == 0)
        .ok_or(RealizationError::Malformed)?;
    if &bytes[..nul] != iri.as_bytes() {
        return Err(RealizationError::WrongIri);
    }
    let mut cur = nul + 1;
    let n = read_u32(bytes, &mut cur)? as usize;
    let mut refs = Vec::with_capacity(n);
    for _ in 0..n {
        let end = cur
            .checked_add(KAPPA71)
            .ok_or(RealizationError::Truncated)?;
        let arr: [u8; KAPPA71] = bytes
            .get(cur..end)
            .ok_or(RealizationError::Truncated)?
            .try_into()
            .map_err(|_| RealizationError::Truncated)?;
        refs.push(Kappa::from_bytes(&arr).map_err(|_| RealizationError::Malformed)?);
        cur = end;
    }
    Ok(refs)
}

fn read_u32(bytes: &[u8], cur: &mut usize) -> Result<u32, RealizationError> {
    let end = cur.checked_add(4).ok_or(RealizationError::Truncated)?;
    let arr: [u8; 4] = bytes
        .get(*cur..end)
        .ok_or(RealizationError::Truncated)?
        .try_into()
        .map_err(|_| RealizationError::Truncated)?;
    *cur = end;
    Ok(u32::from_le_bytes(arr))
}

/// The opaque payload after a realization's embedded operand κ-labels — the
/// inverse of `encode`'s payload region.
fn payload_of(iri: &str, bytes: &[u8]) -> Result<Vec<u8>, RealizationError> {
    let nul = bytes
        .iter()
        .position(|&b| b == 0)
        .ok_or(RealizationError::Malformed)?;
    if &bytes[..nul] != iri.as_bytes() {
        return Err(RealizationError::WrongIri);
    }
    let mut cur = nul + 1;
    let n = read_u32(bytes, &mut cur)? as usize;
    cur = cur
        .checked_add(n * KAPPA71)
        .ok_or(RealizationError::Truncated)?;
    let len = read_u32(bytes, &mut cur)? as usize;
    let end = cur.checked_add(len).ok_or(RealizationError::Truncated)?;
    Ok(bytes
        .get(cur..end)
        .ok_or(RealizationError::Truncated)?
        .to_vec())
}

fn read_kappa(bytes: &[u8], cur: &mut usize) -> Result<Kappa, RealizationError> {
    let end = cur
        .checked_add(KAPPA71)
        .ok_or(RealizationError::Truncated)?;
    let arr: [u8; KAPPA71] = bytes
        .get(*cur..end)
        .ok_or(RealizationError::Truncated)?
        .try_into()
        .map_err(|_| RealizationError::Truncated)?;
    *cur = end;
    Kappa::from_bytes(&arr).map_err(|_| RealizationError::Malformed)
}

fn read_str(bytes: &[u8], cur: &mut usize) -> Result<String, RealizationError> {
    let len = read_u32(bytes, cur)? as usize;
    let end = cur.checked_add(len).ok_or(RealizationError::Truncated)?;
    let slice = bytes.get(*cur..end).ok_or(RealizationError::Truncated)?;
    *cur = end;
    String::from_utf8(slice.to_vec()).map_err(|_| RealizationError::Malformed)
}

/// How a holospace is provisioned. Two paths, one lifecycle (ADR-004). The
/// source is embedded in the [`Holospace`] canonical form, so the holospace
/// identity is a reproducible function of its source (quality scenario QS1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A `.holo` compute artifact, referenced by its content address. The
    /// `.holo` format and its executor are defined by
    /// [hologram](https://github.com/Hologram-Technologies/hologram).
    HoloFile {
        /// The κ-label of the `.holo` artifact.
        artifact: Kappa,
    },
    /// A *Wasm-recompiled userland* — general/system code (the second compute
    /// form, arc42 chapter 8), run by the substrate's `ContainerRuntime` over
    /// its host ABI. This is the *execution surface* a devcontainer's
    /// Linux/POSIX environment recompiles to (ADR-008, resolving RT1); see
    /// [`crate::surface`]. Its identity is content, never an OCI image location
    /// (Law L1).
    Userland {
        /// The κ-label of the entry Wasm code module (the Container ID's code).
        entry: Kappa,
    },
    /// A git repository plus a `devcontainer.json`, per the
    /// [Dev Container](https://containers.dev) specification — "just like
    /// gitpod/codespaces". (Conformance: `CC-4`.)
    ///
    /// The config *selects* a κ-addressed [`Source::Userland`] for its
    /// Linux/POSIX surface (ADR-008): `userland` is the entry module the
    /// runtime boots. Identity covers both the config (the holospace *matches
    /// its source*, `CC-4`) and the userland (so the holospace is bootable and
    /// reproducible). See [`crate::boot::ingest_devcontainer`].
    Devcontainer {
        /// The git repository URL.
        repo: String,
        /// The git reference (branch, tag, or commit).
        reference: String,
        /// Path to the dev container configuration within the repository.
        config_path: String,
        /// The κ-label of the validated `devcontainer.json` content.
        config: Kappa,
        /// The κ-label of the Wasm userland the config selects — the entry
        /// module the runtime boots (the execution surface, ADR-008).
        userland: Kappa,
    },
}

impl Source {
    /// The source descriptor bytes carried in the [`Holospace`] payload.
    fn encode_payload(&self) -> Vec<u8> {
        let mut p = Vec::new();
        match self {
            Source::HoloFile { artifact } => {
                p.push(0u8);
                p.extend_from_slice(artifact.as_array());
            }
            Source::Userland { entry } => {
                p.push(2u8);
                p.extend_from_slice(entry.as_array());
            }
            Source::Devcontainer {
                repo,
                reference,
                config_path,
                config,
                userland,
            } => {
                p.push(1u8);
                p.extend_from_slice(config.as_array());
                p.extend_from_slice(userland.as_array());
                for s in [repo, reference, config_path] {
                    p.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    p.extend_from_slice(s.as_bytes());
                }
            }
        }
        p
    }

    /// Recover a source from its [`Holospace`] payload bytes (the inverse of
    /// [`encode_payload`](Source::encode_payload)).
    fn decode_payload(payload: &[u8]) -> Result<Source, RealizationError> {
        let kind = *payload.first().ok_or(RealizationError::Malformed)?;
        let mut cur = 1usize;
        match kind {
            0 => Ok(Source::HoloFile {
                artifact: read_kappa(payload, &mut cur)?,
            }),
            2 => Ok(Source::Userland {
                entry: read_kappa(payload, &mut cur)?,
            }),
            1 => {
                let config = read_kappa(payload, &mut cur)?;
                let userland = read_kappa(payload, &mut cur)?;
                let repo = read_str(payload, &mut cur)?;
                let reference = read_str(payload, &mut cur)?;
                let config_path = read_str(payload, &mut cur)?;
                Ok(Source::Devcontainer {
                    repo,
                    reference,
                    config_path,
                    config,
                    userland,
                })
            }
            _ => Err(RealizationError::Malformed),
        }
    }

    /// The code-module κ-label this source contributes to the manifest — the
    /// module the runtime boots. For a holo-file it is the `.holo` artifact κ;
    /// for a userland and a devcontainer it is the entry Wasm userland module κ
    /// (the execution surface, ADR-008), so every form is bootable.
    fn code_kappa(&self) -> Kappa {
        match self {
            Source::HoloFile { artifact } => *artifact,
            // A Wasm-recompiled userland: the entry module *is* the code the
            // runtime spawns (the execution surface, ADR-008).
            Source::Userland { entry } => *entry,
            // The devcontainer boots the userland its config selects.
            Source::Devcontainer { userland, .. } => *userland,
        }
    }
}

/// A bootable, κ-addressed environment — the unit holospaces provisions, runs,
/// and manages (arc42 chapter 8, *The holospace*).
///
/// A `Holospace` is a hologram [`Realization`]: its canonical form is IRI-tagged
/// and embeds its operand κ-labels — the [`ContainerManifest`] (its code,
/// the Container ID hologram's runtime spawns) and the [`CapabilitySet`] (the
/// authority it runs under). Its identity is [`Holospace::kappa`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holospace {
    source: Source,
    manifest: Kappa,
    capabilities: Kappa,
}

impl Holospace {
    /// The holospaces realization IRI for a holospace definition.
    pub const IRI: &'static str = "https://uor.foundation/holospaces/realization/holospace";

    /// Compose a holospace from a provisioning source and a capability set.
    ///
    /// The source yields a [`ContainerManifest`] (the Container ID); the
    /// capabilities yield a [`CapabilitySet`]; the holospace embeds both.
    #[must_use]
    pub fn compose(source: Source, capabilities: Capabilities) -> Self {
        let manifest = ContainerManifest {
            code: source.code_kappa(),
            initial_state: empty_kappa(),
            parameters: empty_kappa(),
        }
        .kappa();
        let capabilities = CapabilitySet::new(capabilities).kappa();
        Self {
            source,
            manifest,
            capabilities,
        }
    }

    /// The provisioning source.
    #[must_use]
    pub fn source(&self) -> &Source {
        &self.source
    }

    /// The Container ID — the [`ContainerManifest`] κ hologram's runtime spawns.
    #[must_use]
    pub fn manifest(&self) -> &Kappa {
        &self.manifest
    }

    /// The hologram [`ContainerManifest`] this holospace's Container ID
    /// addresses — its code module κ (from the source) with empty initial
    /// state and parameters. Persisting its canonical form into a `KappaStore`
    /// is what lets hologram's runtime resolve and spawn the holospace.
    #[must_use]
    pub fn container_manifest(&self) -> ContainerManifest {
        ContainerManifest {
            code: self.source.code_kappa(),
            initial_state: empty_kappa(),
            parameters: empty_kappa(),
        }
    }

    /// The [`CapabilitySet`] κ — the authority the holospace runs under.
    #[must_use]
    pub fn capabilities(&self) -> &Kappa {
        &self.capabilities
    }

    /// The holospace's identity: the κ-label of its canonical form (Law L1;
    /// reproducible from its source, QS1).
    #[must_use]
    pub fn kappa(&self) -> Kappa {
        address(&self.canonicalize())
    }

    /// Recover a holospace from its canonical form — the inverse of
    /// [`canonicalize`](Realization::canonicalize). This is what lets a peer
    /// resolve a holospace κ (fetch + verify, Law L5) and then boot it: the
    /// embedded operands give the manifest and capability-set κ, the payload
    /// the provisioning source.
    ///
    /// # Errors
    ///
    /// [`RealizationError`] if the bytes are not a well-formed holospace.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, RealizationError> {
        let refs = <Self as Realization>::references(bytes)?;
        if refs.len() != 2 {
            return Err(RealizationError::Malformed);
        }
        let payload = payload_of(Self::IRI, bytes)?;
        Ok(Self {
            source: Source::decode_payload(&payload)?,
            manifest: refs[0],
            capabilities: refs[1],
        })
    }

    fn parts(&self) -> (Vec<Kappa>, Vec<u8>) {
        (
            alloc_vec(&[self.manifest, self.capabilities]),
            self.source.encode_payload(),
        )
    }
}

fn alloc_vec(items: &[Kappa]) -> Vec<Kappa> {
    items.to_vec()
}

impl Realization for Holospace {
    const IRI: hologram_substrate_core::RealizationId = Holospace::IRI;

    fn canonicalize(&self) -> Vec<u8> {
        let (refs, payload) = self.parts();
        encode(Self::IRI, &refs, &payload)
    }

    fn references(canonical_bytes: &[u8]) -> Result<References, RealizationError> {
        extract_refs(Self::IRI, canonical_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps() -> Capabilities {
        Capabilities {
            storage_roots: Vec::new(),
            storage_quota_bytes: 0,
            network_fetch: false,
            network_announce: false,
            publish_channels: Vec::new(),
            subscribe_channels: Vec::new(),
            memory_max_bytes: 0,
            cpu_time_per_event_ms: 0,
            priority_weight: 0,
        }
    }

    fn devcontainer() -> Source {
        Source::Devcontainer {
            repo: "https://example.invalid/app.git".to_owned(),
            reference: "main".to_owned(),
            config_path: ".devcontainer/devcontainer.json".to_owned(),
            config: address(br#"{"image":"debian:12"}"#),
            userland: address(b"the recompiled userland the config selects"),
        }
    }

    #[test]
    fn axis_tokens_match_the_substrate() {
        assert_eq!(Axis::Blake3.token(), "blake3");
        assert_eq!(Axis::Keccak256.token(), "keccak256");
    }

    #[test]
    fn address_is_blake3_and_verifies_by_rederivation() {
        let k = address(b"holospace-canonical-bytes");
        assert_eq!(k.sigma_axis(), Some("blake3"));
        assert!(verify(b"holospace-canonical-bytes", &k).unwrap());
        assert!(!verify(b"tampered", &k).unwrap());
    }

    #[test]
    fn holospace_identity_is_reproducible() {
        // QS1: the same source + capabilities yield the same holospace κ.
        let a = Holospace::compose(devcontainer(), caps()).kappa();
        let b = Holospace::compose(devcontainer(), caps()).kappa();
        assert_eq!(a, b);
    }

    #[test]
    fn capabilities_are_part_of_identity() {
        let base = Holospace::compose(devcontainer(), caps());
        let mut scoped = caps();
        scoped.memory_max_bytes = 1 << 30;
        let other = Holospace::compose(devcontainer(), scoped);
        assert_ne!(base.kappa(), other.kappa());
    }

    #[test]
    fn references_recover_the_embedded_operands() {
        // SPINE-3: the holospace canonical form embeds its manifest + caps κ.
        let hs = Holospace::compose(devcontainer(), caps());
        let bytes = hs.canonicalize();
        let refs = Holospace::references(&bytes).unwrap();
        assert_eq!(refs, vec![*hs.manifest(), *hs.capabilities()]);
    }

    #[test]
    fn holospace_round_trips_through_its_canonical_form() {
        for hs in [
            Holospace::compose(devcontainer(), caps()),
            Holospace::compose(
                Source::HoloFile {
                    artifact: address(b"a .holo artifact"),
                },
                caps(),
            ),
            Holospace::compose(
                Source::Userland {
                    entry: address(b"a recompiled userland module"),
                },
                caps(),
            ),
        ] {
            let back = Holospace::from_canonical(&hs.canonicalize()).expect("decode");
            assert_eq!(back, hs);
            assert_eq!(back.kappa(), hs.kappa());
            assert_eq!(back.source(), hs.source());
        }
    }

    #[test]
    fn holo_file_and_devcontainer_have_distinct_identity() {
        let holofile = Holospace::compose(
            Source::HoloFile {
                artifact: address(b"a .holo artifact"),
            },
            caps(),
        )
        .kappa();
        let devc = Holospace::compose(devcontainer(), caps()).kappa();
        assert_ne!(holofile, devc);
    }

    #[test]
    fn the_two_compute_forms_are_distinct_at_the_same_code_kappa() {
        // The second compute form (a Wasm userland, ADR-008) is a different
        // holospace from a tensor `.holo` even when both reference the same code
        // κ — the form is part of identity, so the runtime never confuses them.
        let code = address(b"some code module bytes");
        let holofile = Holospace::compose(Source::HoloFile { artifact: code }, caps());
        let userland = Holospace::compose(Source::Userland { entry: code }, caps());
        assert_eq!(
            holofile.manifest(),
            userland.manifest(),
            "same Container ID"
        );
        assert_ne!(
            holofile.kappa(),
            userland.kappa(),
            "but distinct holospace identity (the compute form differs)"
        );
        // The userland source round-trips through the canonical form.
        let back = Holospace::from_canonical(&userland.canonicalize()).unwrap();
        assert_eq!(back.source(), &Source::Userland { entry: code });
    }
}
