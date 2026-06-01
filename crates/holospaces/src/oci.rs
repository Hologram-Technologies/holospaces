//! **OCI ingestion** — a devcontainer's operating-system image as κ-addressed content.
//!
//! Realizes the devcontainer-ingestion path of ADR-009 (arc42 chapter 9) and the
//! provisioning boundary of arc42 chapter 3: a real OCI image — the base image a
//! `devcontainer.json` selects — is *ingested at the boundary* into κ-addressed
//! content the substrate holds (Law L2), not referenced by registry location
//! (Law L1).
//!
//! The fit is exact: an OCI **content digest** (`sha256:…`) *is* a κ-label on the
//! substrate's `sha256` σ-axis. So ingesting an image is verify-by-re-derivation
//! (Law L5) against the registry's own content addressing — each blob is accepted
//! only if re-deriving its `sha256` reproduces the digest the manifest names
//! ([`hologram_substrate_core::verify_kappa_axis`]). holospaces walks the OCI
//! image-layout graph — index → manifest → config + layers
//! (<https://github.com/opencontainers/image-spec>) — verifying and storing every
//! blob, and yields a reproducible image identity (the same image ⇒ the same κ,
//! on any peer). Conformance: `CC-7`/`CC-8` are the disk and import this composes;
//! `CC-10` (arc42 chapter 10) witnesses the ingestion against the OCI image and
//! Dev Container specifications.
//!
//! The booted *behaviour* of the ingested image is the system emulator's
//! (`CC-9`); this module is the ingestion and identity boundary it consumes —
//! the image's layers become the [κ-disk](crate::disk) the emulator reads.

use serde_json::Value;

use hologram_substrate_core::{verify_kappa_axis, KappaStore, StoreError};

use crate::realizations::{address, Kappa};

/// OCI media types holospaces recognises at the ingestion boundary
/// (<https://github.com/opencontainers/image-spec>, `media-types.md`).
mod media {
    pub const INDEX: &str = "application/vnd.oci.image.index.v1+json";
    pub const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
    pub const CONFIG: &str = "application/vnd.oci.image.config.v1+json";
    /// Layer media types (gzip / plain / zstd), incl. non-distributable variants.
    pub const LAYER_PREFIXES: [&str; 2] = [
        "application/vnd.oci.image.layer.",
        "application/vnd.oci.image.layer.nondistributable.",
    ];
}

/// The holospaces identity IRI for an ingested OCI image (its reproducible κ is a
/// function of the image's content digest — Law L1).
const IMAGE_IRI: &str = "https://uor.foundation/holospaces/realization/oci-image";

/// A real OCI image ingested into the substrate as κ-addressed content. Each
/// field is the substrate (blake3) store κ of a blob that was verified by
/// re-derivation against its OCI `sha256` digest on the way in (Law L5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IngestedImage {
    digest: String,
    manifest: Kappa,
    config: Kappa,
    layers: Vec<Kappa>,
    layer_media_types: Vec<String>,
    identity: Kappa,
}

impl IngestedImage {
    /// The OCI manifest digest (`sha256:…`) — itself a κ-label on the `sha256`
    /// axis (the registry's content address, now verified into the substrate).
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The substrate κ of the image manifest blob (its content in this peer's
    /// store).
    #[must_use]
    pub fn manifest(&self) -> &Kappa {
        &self.manifest
    }

    /// The substrate κ of the image config blob.
    #[must_use]
    pub fn config(&self) -> &Kappa {
        &self.config
    }

    /// The substrate κ of each layer blob, in order — the content the
    /// [κ-disk](crate::disk) the emulator boots is assembled from.
    #[must_use]
    pub fn layers(&self) -> &[Kappa] {
        &self.layers
    }

    /// The OCI media type of each layer, in the same order as [`Self::layers`]
    /// — selects the decompression in the Layer Assembler (plain `tar`, gzip).
    #[must_use]
    pub fn layer_media_types(&self) -> &[String] {
        &self.layer_media_types
    }

    /// The image's reproducible identity: a κ derived from its OCI manifest
    /// digest (which, by OCI's Merkle structure, commits to the config and every
    /// layer). The same image yields the same identity on any peer (Law L1).
    #[must_use]
    pub fn identity(&self) -> Kappa {
        self.identity
    }
}

/// Ingest a real OCI image-layout into `store` as κ-addressed content, verifying
/// every blob by re-derivation against its OCI `sha256` digest (Law L5).
///
/// `oci_layout` and `index_json` are the layout's `oci-layout` and `index.json`
/// bytes; `blob` resolves a descriptor digest (`"sha256:…"`) to its content
/// (the boundary read — once; everything after is κ-addressed). The graph walked
/// is index → manifest → config + layers, per the OCI image specification.
///
/// # Errors
///
/// [`OciError`] if the layout/index/manifest is malformed or has an unexpected
/// media type, a referenced blob is absent, a blob fails re-derivation against
/// its digest (a corrupt or forged image — Law L5), or the store rejects a write.
pub fn ingest_image<F>(
    store: &dyn KappaStore,
    oci_layout: &[u8],
    index_json: &[u8],
    mut blob: F,
) -> Result<IngestedImage, OciError>
where
    F: FnMut(&str) -> Option<Vec<u8>>,
{
    // The layout marker (OCI image-layout spec): imageLayoutVersion "1.0.0".
    let layout: Value = serde_json::from_slice(oci_layout).map_err(|_| OciError::BadLayout)?;
    if layout.get("imageLayoutVersion").and_then(Value::as_str) != Some("1.0.0") {
        return Err(OciError::BadLayout);
    }

    // The index selects the image manifest (the first manifest descriptor).
    let index: Value = serde_json::from_slice(index_json).map_err(|_| OciError::BadIndex)?;
    if let Some(mt) = index.get("mediaType").and_then(Value::as_str) {
        if mt != media::INDEX {
            return Err(OciError::UnexpectedMediaType(mt.to_owned()));
        }
    }
    let manifest_desc = index
        .get("manifests")
        .and_then(Value::as_array)
        .and_then(|m| m.first())
        .ok_or(OciError::NoManifest)?;
    let manifest_digest = descriptor_digest(manifest_desc)?;
    expect_media(manifest_desc, media::MANIFEST)?;

    // Fetch + verify (Law L5) + store the manifest.
    let (manifest_bytes, manifest_k) = fetch_verified(store, &mut blob, &manifest_digest)?;
    let manifest: Value =
        serde_json::from_slice(&manifest_bytes).map_err(|_| OciError::BadManifest)?;

    // Config blob.
    let config_desc = manifest.get("config").ok_or(OciError::BadManifest)?;
    expect_media(config_desc, media::CONFIG)?;
    let config_digest = descriptor_digest(config_desc)?;
    let (_, config_k) = fetch_verified(store, &mut blob, &config_digest)?;

    // Layer blobs, in order.
    let layer_descs = manifest
        .get("layers")
        .and_then(Value::as_array)
        .ok_or(OciError::BadManifest)?;
    let mut layers = Vec::with_capacity(layer_descs.len());
    let mut layer_media_types = Vec::with_capacity(layer_descs.len());
    for desc in layer_descs {
        expect_layer_media(desc)?;
        let mt = desc
            .get("mediaType")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let digest = descriptor_digest(desc)?;
        let (_, layer_k) = fetch_verified(store, &mut blob, &digest)?;
        layers.push(layer_k);
        layer_media_types.push(mt);
    }

    let identity = image_identity(&manifest_digest);
    Ok(IngestedImage {
        digest: manifest_digest,
        manifest: manifest_k,
        config: config_k,
        layers,
        layer_media_types,
        identity,
    })
}

/// Resolve a descriptor digest, re-derive the blob's `sha256` against it (Law
/// L5), and store the verified bytes (blake3, the substrate's native axis).
fn fetch_verified<F>(
    store: &dyn KappaStore,
    blob: &mut F,
    digest: &str,
) -> Result<(Vec<u8>, Kappa), OciError>
where
    F: FnMut(&str) -> Option<Vec<u8>>,
{
    let bytes = blob(digest).ok_or_else(|| OciError::MissingBlob(digest.to_owned()))?;
    // The OCI digest string *is* a κ-label on the sha256 axis: re-derivation must
    // reproduce it exactly, or the blob is corrupt/forged (Law L5).
    match verify_kappa_axis(&bytes, digest.as_bytes()) {
        Ok(true) => {}
        Ok(false) => return Err(OciError::DigestMismatch(digest.to_owned())),
        Err(_) => return Err(OciError::BadDigest(digest.to_owned())),
    }
    let k = store.put("blake3", &bytes).map_err(OciError::Store)?;
    Ok((bytes, k))
}

/// An OCI descriptor's `digest` field (`"sha256:…"`).
fn descriptor_digest(desc: &Value) -> Result<String, OciError> {
    desc.get("digest")
        .and_then(Value::as_str)
        .filter(|d| d.starts_with("sha256:"))
        .map(str::to_owned)
        .ok_or(OciError::BadManifest)
}

/// Require a descriptor's `mediaType` to equal `want`.
fn expect_media(desc: &Value, want: &str) -> Result<(), OciError> {
    match desc.get("mediaType").and_then(Value::as_str) {
        Some(mt) if mt == want => Ok(()),
        Some(mt) => Err(OciError::UnexpectedMediaType(mt.to_owned())),
        None => Err(OciError::BadManifest),
    }
}

/// Require a descriptor's `mediaType` to be an OCI image layer type.
fn expect_layer_media(desc: &Value) -> Result<(), OciError> {
    match desc.get("mediaType").and_then(Value::as_str) {
        Some(mt) if media::LAYER_PREFIXES.iter().any(|p| mt.starts_with(p)) => Ok(()),
        Some(mt) => Err(OciError::UnexpectedMediaType(mt.to_owned())),
        None => Err(OciError::BadManifest),
    }
}

/// The reproducible image identity κ from its manifest digest (Law L1).
fn image_identity(manifest_digest: &str) -> Kappa {
    let mut canonical = Vec::with_capacity(IMAGE_IRI.len() + 1 + manifest_digest.len());
    canonical.extend_from_slice(IMAGE_IRI.as_bytes());
    canonical.push(0);
    canonical.extend_from_slice(manifest_digest.as_bytes());
    address(&canonical)
}

/// The reproducible identity of a *devcontainer source* — its validated
/// `devcontainer.json` bound to its ingested base image (arc42 chapter 3:
/// "a git repository with a valid `devcontainer.json` and its operating-system
/// image"). The config is validated against the Dev Container specification
/// ([`crate::boot::devcontainer::parse`], `CC-4`); the identity is a content
/// function of both, so the same source yields the same holospace identity on
/// any peer (Law L1; QS1/Q4). This is the κ the system emulator (`CC-9`) boots.
///
/// # Errors
///
/// [`crate::boot::devcontainer::DevcontainerError`] if the config is not Dev
/// Container spec-conformant.
pub fn devcontainer_source_identity(
    config_json: &[u8],
    image: &IngestedImage,
) -> Result<Kappa, crate::boot::devcontainer::DevcontainerError> {
    crate::boot::devcontainer::parse(config_json)?;
    let cfg_k = address(config_json);
    const IRI: &str = "https://uor.foundation/holospaces/realization/devcontainer-source";
    let mut canonical = Vec::with_capacity(IRI.len() + 1 + 71 + 71);
    canonical.extend_from_slice(IRI.as_bytes());
    canonical.push(0);
    canonical.extend_from_slice(cfg_k.as_array());
    canonical.extend_from_slice(image.identity().as_array());
    Ok(address(&canonical))
}

/// Why an OCI image could not be ingested.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OciError {
    /// The `oci-layout` marker is missing or not version `1.0.0`.
    BadLayout,
    /// `index.json` is malformed.
    BadIndex,
    /// The index declares no image manifest.
    NoManifest,
    /// An image manifest is malformed (missing config/layers/digest).
    BadManifest,
    /// A descriptor's media type is not the expected OCI type.
    UnexpectedMediaType(String),
    /// A referenced blob is absent from the layout.
    MissingBlob(String),
    /// A blob does not re-derive to its OCI digest (corrupt or forged, Law L5).
    DigestMismatch(String),
    /// A digest is not a supported σ-axis label.
    BadDigest(String),
    /// The store rejected a write.
    Store(StoreError),
}

impl core::fmt::Display for OciError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OciError::BadLayout => f.write_str("oci-layout marker missing or unsupported version"),
            OciError::BadIndex => f.write_str("index.json is malformed"),
            OciError::NoManifest => f.write_str("OCI index declares no image manifest"),
            OciError::BadManifest => f.write_str("OCI image manifest is malformed"),
            OciError::UnexpectedMediaType(mt) => write!(f, "unexpected OCI media type '{mt}'"),
            OciError::MissingBlob(d) => write!(f, "OCI blob {d} is absent from the layout"),
            OciError::DigestMismatch(d) => {
                write!(f, "OCI blob does not re-derive to its digest {d} (L5)")
            }
            OciError::BadDigest(d) => write!(f, "OCI digest {d} is not a supported σ-axis label"),
            OciError::Store(e) => write!(f, "store error ingesting an OCI blob: {e:?}"),
        }
    }
}

impl std::error::Error for OciError {}

#[cfg(test)]
mod tests {
    use super::*;
    use hologram_store_mem::MemKappaStore;
    use std::collections::HashMap;

    // A tiny hand-built but spec-shaped OCI layout with real sha256 digests, so
    // the unit tests are self-contained (the real BuildKit artifact is exercised
    // by the CC-10 integration witness).
    fn sha256_label(bytes: &[u8]) -> String {
        let label = hologram_substrate_core::address_bytes_axis("sha256", bytes).unwrap();
        String::from_utf8(label).unwrap()
    }

    fn build_layout() -> (Vec<u8>, Vec<u8>, HashMap<String, Vec<u8>>) {
        let mut blobs = HashMap::new();

        let config =
            br#"{"architecture":"amd64","os":"linux","rootfs":{"type":"layers","diff_ids":[]}}"#
                .to_vec();
        let config_d = sha256_label(&config);
        blobs.insert(config_d.clone(), config.clone());

        let layer = b"a real (uncompressed) layer tarball stand-in".to_vec();
        let layer_d = sha256_label(&layer);
        blobs.insert(layer_d.clone(), layer.clone());

        let manifest = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","config":{{"mediaType":"{}","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":{}}}]}}"#,
            media::MANIFEST, media::CONFIG, config_d, config.len(), layer_d, layer.len()
        ).into_bytes();
        let manifest_d = sha256_label(&manifest);
        blobs.insert(manifest_d.clone(), manifest.clone());

        let index = format!(
            r#"{{"schemaVersion":2,"mediaType":"{}","manifests":[{{"mediaType":"{}","digest":"{}","size":{}}}]}}"#,
            media::INDEX, media::MANIFEST, manifest_d, manifest.len()
        ).into_bytes();
        let layout = br#"{"imageLayoutVersion":"1.0.0"}"#.to_vec();
        (layout, index, blobs)
    }

    #[test]
    fn ingests_and_verifies_every_blob_by_re_derivation() {
        let (layout, index, blobs) = build_layout();
        let store = MemKappaStore::new();
        let img = ingest_image(&store, &layout, &index, |d| blobs.get(d).cloned()).unwrap();
        assert_eq!(img.layers().len(), 1);
        assert!(store.contains(img.manifest()));
        assert!(store.contains(img.config()));
        assert!(store.contains(&img.layers()[0]));
        // Reproducible identity (Law L1).
        let store2 = MemKappaStore::new();
        let img2 = ingest_image(&store2, &layout, &index, |d| blobs.get(d).cloned()).unwrap();
        assert_eq!(img.identity(), img2.identity());
        assert_eq!(img.digest(), img2.digest());
    }

    #[test]
    fn a_forged_blob_is_refused() {
        let (layout, index, mut blobs) = build_layout();
        // Tamper a blob: serve different bytes under a digest it no longer matches.
        let some_digest = blobs.keys().next().unwrap().clone();
        blobs.insert(some_digest.clone(), b"tampered content".to_vec());
        let store = MemKappaStore::new();
        let err = ingest_image(&store, &layout, &index, |d| blobs.get(d).cloned()).unwrap_err();
        assert!(
            matches!(err, OciError::DigestMismatch(_)),
            "a blob that does not re-derive to its OCI digest is refused (L5), got {err:?}"
        );
    }

    #[test]
    fn a_missing_blob_is_refused() {
        let (layout, index, _blobs) = build_layout();
        let store = MemKappaStore::new();
        let err = ingest_image(&store, &layout, &index, |_| None).unwrap_err();
        assert!(matches!(err, OciError::MissingBlob(_)));
    }

    #[test]
    fn devcontainer_source_identity_is_reproducible_and_validates_the_config() {
        let (layout, index, blobs) = build_layout();
        let store = MemKappaStore::new();
        let img = ingest_image(&store, &layout, &index, |d| blobs.get(d).cloned()).unwrap();
        let cfg = br#"{"name":"app","image":"debian:12"}"#;
        let a = devcontainer_source_identity(cfg, &img).unwrap();
        let b = devcontainer_source_identity(cfg, &img).unwrap();
        assert_eq!(a, b, "same source ⇒ same κ (QS1)");
        // A config declaring two image sources is rejected (Dev Container spec).
        let bad = br#"{"image":"debian:12","build":{"dockerfile":"Dockerfile"}}"#;
        assert!(devcontainer_source_identity(bad, &img).is_err());
    }
}
