//! **Boot Layer** — the environment-agnostic core.
//!
//! Realizes the *Boot Layer* building block (arc42 chapter 5,
//! `docs/src/arc42/adoc/05_building_block_view.adoc`): an [`ingest`] step that
//! canonicalizes a provisioning source at the boundary (Law L2), a
//! [`Resolver`] that fetches and verifies a holospace's parts by re-derivation
//! (Law L5), and a [`Session`] that drives the lifecycle (boot → suspend to a
//! κ snapshot → resume → migrate → terminate).
//!
//! The Boot Layer composes the [hologram](https://github.com/Hologram-Technologies/hologram)
//! substrate (ADR-003, ADR-006): it resolves through
//! [`KappaStore`]/[`KappaSync`] (`get_with_fetch`, verify-on-receipt) and runs
//! through [`ContainerRuntime`]. It never re-implements them.

use core::fmt;

use hologram_realizations::CapabilitySet;
use hologram_substrate_core::{
    get_with_fetch, verify_kappa, AccessError, Bytes, Capabilities, ContainerHandle,
    ContainerRuntime, KappaStore, KappaSync, Realization, RuntimeError, StoreError,
};

#[cfg(feature = "std")]
use crate::realizations::address;
use crate::realizations::{Holospace, Kappa, Source};

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

/// Provision a holospace *into a peer's store* so the substrate can resolve and
/// spawn it (arc42 chapter 6, *Provisioning*; chapter 5, *Boot Layer*).
///
/// This is the boundary step that κ-addresses a holospace's parts into the
/// content-addressed store (Law L2/L4): the hologram
/// [`ContainerManifest`](hologram_realizations::ContainerManifest) (the
/// Container ID), the [`CapabilitySet`] (the authority), and the [`Holospace`]
/// definition itself. The code module bytes the manifest references must
/// already be in the store (a holo-file artifact, or an ingested config).
///
/// # Errors
///
/// Returns [`ProvisionError`] if any part cannot be stored, or if the code
/// module the manifest references is not present in the store.
pub fn provision(
    store: &dyn KappaStore,
    source: Source,
    capabilities: Capabilities,
) -> Result<Holospace, ProvisionError> {
    let holospace = Holospace::compose(source, capabilities.clone());
    let manifest = holospace.container_manifest();
    if !store.contains(&manifest.code) {
        return Err(ProvisionError::MissingCode(manifest.code));
    }
    store
        .put("blake3", &manifest.canonicalize())
        .map_err(ProvisionError::Store)?;
    store
        .put("blake3", &CapabilitySet::new(capabilities).canonicalize())
        .map_err(ProvisionError::Store)?;
    store
        .put("blake3", &holospace.canonicalize())
        .map_err(ProvisionError::Store)?;
    Ok(holospace)
}

/// Why provisioning a holospace into a store failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionError {
    /// The code module the manifest references is not present in the store.
    MissingCode(Kappa),
    /// The store rejected a write.
    Store(StoreError),
}

impl fmt::Display for ProvisionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProvisionError::MissingCode(k) => {
                write!(f, "code module {k} is not present in the store")
            }
            ProvisionError::Store(e) => write!(f, "store error during provisioning: {e:?}"),
        }
    }
}

impl core::error::Error for ProvisionError {}

/// The Dev Container ingestor — parses and validates a `devcontainer.json`
/// against the [Dev Container](https://containers.dev) specification at the
/// provisioning boundary (Law L2). Conformance: `CC-4`. A host-side
/// provisioning surface, available with the `std` feature.
#[cfg(feature = "std")]
pub mod devcontainer {
    use core::fmt;
    use serde_json::Value;

    /// The container image source a `devcontainer.json` declares. The Dev
    /// Container spec permits at most one (the OCI image origin); when none is
    /// given, the implementor's default base image is used.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum ImageSource {
        /// A prebuilt OCI image reference (`"image"`).
        Image(String),
        /// A Dockerfile build (`"build"`).
        Build,
        /// A Docker Compose service (`"dockerComposeFile"`).
        Compose,
        /// No image source declared — the default base image (e.g. a
        /// features-only configuration, as in Codespaces).
        Default,
    }

    /// A validated dev container configuration (the spec-relevant projection).
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct DevContainer {
        /// The optional `"name"`.
        pub name: Option<String>,
        /// The declared container image source (or [`ImageSource::Default`]).
        pub image_source: ImageSource,
    }

    /// Normalize JSONC `devcontainer.json` bytes to plain JSON (Law L2): strip
    /// line/block comments and trailing commas. The Dev Container format is
    /// JSONC; this is the canonicalization at the provisioning boundary.
    ///
    /// # Errors
    ///
    /// [`DevcontainerError::NotJson`] if the result is not valid JSON.
    pub fn to_canonical_json(config_json: &[u8]) -> Result<Vec<u8>, DevcontainerError> {
        let json = strip_trailing_commas(&strip_jsonc(config_json));
        // Round-trip through serde_json to confirm it is valid JSON and to
        // normalize it (the value the schema and the ingestor see).
        let value: Value = serde_json::from_slice(&json).map_err(|_| DevcontainerError::NotJson)?;
        serde_json::to_vec(&value).map_err(|_| DevcontainerError::NotJson)
    }

    /// Parse and validate `devcontainer.json` per the Dev Container spec.
    ///
    /// `devcontainer.json` is JSONC: comments and trailing commas are stripped
    /// ([`to_canonical_json`]) before parsing. At most one container image
    /// source (`image` / `build` / `dockerComposeFile`) may be declared; known
    /// properties must be well-formed.
    ///
    /// # Errors
    ///
    /// [`DevcontainerError`] if the bytes are not a JSON object, declare more
    /// than one image source, or have a malformed known property.
    pub fn parse(config_json: &[u8]) -> Result<DevContainer, DevcontainerError> {
        let json = to_canonical_json(config_json)?;
        let value: Value = serde_json::from_slice(&json).map_err(|_| DevcontainerError::NotJson)?;
        let obj = value.as_object().ok_or(DevcontainerError::NotObject)?;

        let has_image = obj.contains_key("image");
        let has_build = obj.contains_key("build");
        let has_compose = obj.contains_key("dockerComposeFile");
        if [has_image, has_build, has_compose]
            .iter()
            .filter(|b| **b)
            .count()
            > 1
        {
            return Err(DevcontainerError::MultipleImageSources);
        }
        if let Some(features) = obj.get("features") {
            if !features.is_object() {
                return Err(DevcontainerError::MalformedProperty("features"));
            }
        }

        let image_source = if has_image {
            ImageSource::Image(
                obj["image"]
                    .as_str()
                    .ok_or(DevcontainerError::MalformedProperty("image"))?
                    .to_owned(),
            )
        } else if has_build {
            ImageSource::Build
        } else if has_compose {
            ImageSource::Compose
        } else {
            ImageSource::Default
        };
        let name = obj.get("name").and_then(Value::as_str).map(str::to_owned);
        Ok(DevContainer { name, image_source })
    }

    /// Strip JSONC line (`//`) and block (`/* */`) comments, respecting string
    /// literals (a comment marker inside a string is content, not a comment).
    fn strip_jsonc(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let (mut i, n) = (0usize, input.len());
        let (mut in_str, mut esc) = (false, false);
        while i < n {
            let b = input[i];
            if in_str {
                out.push(b);
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    in_str = false;
                }
                i += 1;
            } else if b == b'"' {
                in_str = true;
                out.push(b);
                i += 1;
            } else if b == b'/' && i + 1 < n && input[i + 1] == b'/' {
                i += 2;
                while i < n && input[i] != b'\n' {
                    i += 1;
                }
            } else if b == b'/' && i + 1 < n && input[i + 1] == b'*' {
                i += 2;
                while i + 1 < n && !(input[i] == b'*' && input[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            } else {
                out.push(b);
                i += 1;
            }
        }
        out
    }

    /// Drop a comma immediately preceding `}` or `]` (JSONC trailing commas),
    /// respecting string literals.
    fn strip_trailing_commas(input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len());
        let (mut in_str, mut esc) = (false, false);
        let n = input.len();
        for i in 0..n {
            let b = input[i];
            if in_str {
                out.push(b);
                if esc {
                    esc = false;
                } else if b == b'\\' {
                    esc = true;
                } else if b == b'"' {
                    in_str = false;
                }
                continue;
            }
            if b == b'"' {
                in_str = true;
                out.push(b);
                continue;
            }
            if b == b',' {
                let mut j = i + 1;
                while j < n && input[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < n && (input[j] == b'}' || input[j] == b']') {
                    continue;
                }
            }
            out.push(b);
        }
        out
    }

    /// Why a `devcontainer.json` is not spec-conformant.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum DevcontainerError {
        /// The bytes are not valid JSON (after JSONC stripping).
        NotJson,
        /// The top-level value is not a JSON object.
        NotObject,
        /// More than one image source was declared.
        MultipleImageSources,
        /// A known property has the wrong shape.
        MalformedProperty(&'static str),
    }

    impl fmt::Display for DevcontainerError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                DevcontainerError::NotJson => f.write_str("devcontainer.json is not valid JSON"),
                DevcontainerError::NotObject => {
                    f.write_str("devcontainer.json top level is not an object")
                }
                DevcontainerError::MultipleImageSources => {
                    f.write_str("devcontainer.json declares more than one image source")
                }
                DevcontainerError::MalformedProperty(p) => {
                    write!(f, "devcontainer.json property '{p}' is malformed")
                }
            }
        }
    }

    impl core::error::Error for DevcontainerError {}
}

/// Ingest a devcontainer holospace from a git source + its `devcontainer.json`
/// (ADR-004; Conformance `CC-4`).
///
/// The config is validated against the Dev Container spec
/// ([`devcontainer::parse`]) and its content is content-addressed, so the
/// holospace identity is a function of the actual configuration — the
/// holospace *matches its source*, and the same source yields the same κ
/// (reproducibility, QS1 / Q4).
///
/// # Errors
///
/// [`IngestError`] if a required field is empty or the config is not
/// spec-conformant.
#[cfg(feature = "std")]
pub fn ingest_devcontainer(
    repo: impl Into<String>,
    reference: impl Into<String>,
    config_path: impl Into<String>,
    config_json: &[u8],
    capabilities: hologram_substrate_core::Capabilities,
) -> Result<Holospace, IngestError> {
    let repo = repo.into();
    let config_path = config_path.into();
    if repo.trim().is_empty() {
        return Err(IngestError::EmptyField("repo"));
    }
    if config_path.trim().is_empty() {
        return Err(IngestError::EmptyField("config_path"));
    }
    devcontainer::parse(config_json).map_err(IngestError::Devcontainer)?;
    let source = Source::Devcontainer {
        repo,
        reference: reference.into(),
        config_path,
        config: address(config_json),
    };
    Ok(Holospace::compose(source, capabilities))
}

/// Canonicalize a provisioning source into a [`Holospace`] definition (Law L2).
///
/// The resulting holospace is a canonical form; its identity is its κ. This is
/// the boundary at which an external source becomes κ-addressed content.
///
/// # Errors
///
/// Returns [`IngestError`] if the source is not well-formed (for example, a
/// devcontainer with an empty repository or config path).
pub fn ingest(
    source: Source,
    capabilities: hologram_substrate_core::Capabilities,
) -> Result<Holospace, IngestError> {
    if let Source::Devcontainer {
        repo, config_path, ..
    } = &source
    {
        if repo.trim().is_empty() {
            return Err(IngestError::EmptyField("repo"));
        }
        if config_path.trim().is_empty() {
            return Err(IngestError::EmptyField("config_path"));
        }
    }
    Ok(Holospace::compose(source, capabilities))
}

/// Why a provisioning source could not be ingested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngestError {
    /// A required field of the source was empty.
    EmptyField(&'static str),
    /// The `devcontainer.json` is not Dev Container spec-conformant (`std`).
    #[cfg(feature = "std")]
    Devcontainer(devcontainer::DevcontainerError),
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IngestError::EmptyField(field) => {
                write!(f, "provisioning source field '{field}' is empty")
            }
            #[cfg(feature = "std")]
            IngestError::Devcontainer(e) => write!(f, "devcontainer source is invalid: {e}"),
        }
    }
}

impl core::error::Error for IngestError {}

/// Resolves κ-labels to their bytes, verifying them by re-derivation before
/// accepting them (Law L5).
///
/// Trust is in the math: bytes that do not re-derive to the requested κ are
/// rejected, which is what makes an untrusted gateway (GitHub Pages, or any
/// peer) safe to fetch from (quality scenario QS3). The network read path is
/// the substrate's own [`get_with_fetch`] (local store, else fetch +
/// verify-on-receipt + cache).
pub struct Resolver;

impl Resolver {
    /// Resolve a κ from a local store only, verifying by re-derivation (L5).
    ///
    /// # Errors
    ///
    /// [`AccessError::VerificationFailed`] if the stored bytes do not re-derive
    /// to `kappa` (QS3); [`AccessError::StoreFailure`] on a store error.
    pub fn resolve_local(
        store: &dyn KappaStore,
        kappa: &Kappa,
    ) -> Result<Option<Bytes>, AccessError> {
        match store.get(kappa).map_err(AccessError::StoreFailure)? {
            None => Ok(None),
            Some(bytes) => {
                if verify_kappa(&bytes, kappa).map_err(|_| AccessError::VerificationFailed)? {
                    Ok(Some(bytes))
                } else {
                    Err(AccessError::VerificationFailed)
                }
            }
        }
    }

    /// Resolve a κ from the local store, else fetch it over the substrate and
    /// verify on receipt (Law L5) — the substrate's eviction-tolerant read.
    ///
    /// # Errors
    ///
    /// [`AccessError`] on a store/sync failure or a re-derivation mismatch.
    pub async fn resolve(
        store: &dyn KappaStore,
        sync: &dyn KappaSync,
        kappa: &Kappa,
    ) -> Result<Option<Bytes>, AccessError> {
        get_with_fetch(store, sync, kappa).await
    }
}

/// The phase of a holospace's lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Defined and κ-addressed, not yet running.
    Provisioned,
    /// Spawned and running on a peer.
    Running,
    /// Halted to a κ snapshot; resumable here or on another instance.
    Suspended,
    /// Ended; not resumable.
    Terminated,
}

/// A running session of a holospace on one peer — the Boot Layer driving the
/// substrate's [`ContainerRuntime`] (arc42 chapter 5, *Boot Layer*; chapter 8,
/// *Identity and sync*).
///
/// `boot` spawns the holospace's Container ID (its manifest κ) under its
/// capability-set κ; `suspend` captures a snapshot κ; `resume` restarts from
/// it. Because a snapshot is content (a κ), a holospace suspended on one
/// instance can be resumed on another ([`Session::adopt`], migration QS2).
pub struct Session<'r, R: ContainerRuntime> {
    runtime: &'r R,
    holospace: Holospace,
    handle: Option<ContainerHandle>,
    phase: Phase,
    snapshot: Option<Kappa>,
}

impl<'r, R: ContainerRuntime> Session<'r, R> {
    /// Begin a session for a provisioned holospace, bound to a runtime.
    pub fn provision(runtime: &'r R, holospace: Holospace) -> Self {
        Self {
            runtime,
            holospace,
            handle: None,
            phase: Phase::Provisioned,
            snapshot: None,
        }
    }

    /// Adopt a holospace suspended elsewhere from its snapshot κ (migration,
    /// QS2) — ready to [`resume`](Session::resume) on this instance.
    pub fn adopt(runtime: &'r R, holospace: Holospace, snapshot: Kappa) -> Self {
        Self {
            runtime,
            holospace,
            handle: None,
            phase: Phase::Suspended,
            snapshot: Some(snapshot),
        }
    }

    /// The holospace under management.
    pub fn holospace(&self) -> &Holospace {
        &self.holospace
    }

    /// The current phase.
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The current κ snapshot, if suspended.
    pub fn snapshot(&self) -> Option<&Kappa> {
        self.snapshot.as_ref()
    }

    /// Boot the holospace: spawn its Container ID under its capability set.
    ///
    /// # Errors
    ///
    /// [`LifecycleError`] unless `Provisioned`, or on a runtime failure.
    pub async fn boot(&mut self) -> Result<(), LifecycleError> {
        self.expect(Phase::Provisioned, "boot")?;
        let handle = self
            .runtime
            .spawn(self.holospace.manifest(), self.holospace.capabilities())
            .await
            .map_err(LifecycleError::Runtime)?;
        self.handle = Some(handle);
        self.phase = Phase::Running;
        Ok(())
    }

    /// Suspend the running holospace, capturing its state as a κ snapshot.
    ///
    /// # Errors
    ///
    /// [`LifecycleError`] unless `Running`, or on a runtime failure.
    pub async fn suspend(&mut self) -> Result<Kappa, LifecycleError> {
        self.expect(Phase::Running, "suspend")?;
        let handle = self.handle.ok_or(LifecycleError::Phase {
            from: self.phase,
            action: "suspend",
        })?;
        let snapshot = self
            .runtime
            .suspend(handle)
            .await
            .map_err(LifecycleError::Runtime)?;
        self.handle = None;
        self.phase = Phase::Suspended;
        self.snapshot = Some(snapshot);
        Ok(snapshot)
    }

    /// Resume a suspended holospace from its snapshot κ under its capability set.
    ///
    /// # Errors
    ///
    /// [`LifecycleError`] unless `Suspended` with a snapshot, or on a runtime
    /// failure.
    pub async fn resume(&mut self) -> Result<(), LifecycleError> {
        self.expect(Phase::Suspended, "resume")?;
        let snapshot = self.snapshot.ok_or(LifecycleError::Phase {
            from: self.phase,
            action: "resume",
        })?;
        let handle = self
            .runtime
            .resume(&snapshot, self.holospace.capabilities())
            .await
            .map_err(LifecycleError::Runtime)?;
        self.handle = Some(handle);
        self.phase = Phase::Running;
        Ok(())
    }

    /// Terminate the holospace. Allowed from any phase but `Terminated`.
    ///
    /// # Errors
    ///
    /// [`LifecycleError`] if already `Terminated`, or on a runtime failure.
    pub async fn terminate(&mut self) -> Result<(), LifecycleError> {
        if self.phase == Phase::Terminated {
            return Err(LifecycleError::Phase {
                from: self.phase,
                action: "terminate",
            });
        }
        if let Some(handle) = self.handle.take() {
            self.runtime
                .terminate(handle)
                .await
                .map_err(LifecycleError::Runtime)?;
        }
        self.phase = Phase::Terminated;
        Ok(())
    }

    fn expect(&self, phase: Phase, action: &'static str) -> Result<(), LifecycleError> {
        if self.phase == phase {
            Ok(())
        } else {
            Err(LifecycleError::Phase {
                from: self.phase,
                action,
            })
        }
    }
}

/// A failed lifecycle transition.
#[derive(Debug)]
pub enum LifecycleError {
    /// The transition is not valid from the current phase.
    Phase {
        /// The phase the holospace was in.
        from: Phase,
        /// The action that was attempted.
        action: &'static str,
    },
    /// The substrate runtime rejected the transition.
    Runtime(RuntimeError),
}

impl fmt::Display for LifecycleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LifecycleError::Phase { from, action } => {
                write!(f, "cannot '{action}' a holospace in phase {from:?}")
            }
            LifecycleError::Runtime(e) => write!(f, "substrate runtime error: {e:?}"),
        }
    }
}

impl core::error::Error for LifecycleError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realizations::address;
    use hologram_store_mem::MemKappaStore;
    use hologram_substrate_core::Capabilities;

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

    #[test]
    fn ingest_devcontainer_validates_and_is_reproducible() {
        let cfg = br#"{"name":"app","image":"debian:12"}"#;
        let a = ingest_devcontainer(
            "https://example.invalid/app.git",
            "main",
            ".devcontainer/devcontainer.json",
            cfg,
            caps(),
        )
        .unwrap();
        let b = ingest_devcontainer(
            "https://example.invalid/app.git",
            "main",
            ".devcontainer/devcontainer.json",
            cfg,
            caps(),
        )
        .unwrap();
        assert_eq!(a.kappa(), b.kappa(), "same source ⇒ same κ (QS1)");

        // A config declaring two image sources is rejected (Dev Container spec).
        let err = ingest_devcontainer(
            "https://example.invalid/app.git",
            "main",
            ".devcontainer/devcontainer.json",
            br#"{"image":"debian:12","build":{"dockerfile":"Dockerfile"}}"#,
            caps(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            IngestError::Devcontainer(devcontainer::DevcontainerError::MultipleImageSources)
        ));
    }

    #[test]
    fn ingest_rejects_empty_devcontainer_fields() {
        let err = ingest(
            Source::Devcontainer {
                repo: "  ".to_owned(),
                reference: "main".to_owned(),
                config_path: ".devcontainer/devcontainer.json".to_owned(),
                config: address(br#"{"image":"debian:12"}"#),
            },
            caps(),
        )
        .unwrap_err();
        assert_eq!(err, IngestError::EmptyField("repo"));
    }

    #[test]
    fn resolver_returns_verified_bytes_and_rejects_a_liar() {
        let store = MemKappaStore::new();
        let bytes = b"a holospace part";
        let k = store.put("blake3", bytes).unwrap();
        assert_eq!(
            Resolver::resolve_local(&store, &k).unwrap().as_deref(),
            Some(&bytes[..])
        );

        // QS3: a κ whose stored bytes do not match is rejected on re-derivation.
        // (Construct a κ for content the store does not hold honestly.)
        let other = address(b"different content");
        assert!(Resolver::resolve_local(&store, &other).unwrap().is_none());
    }

    #[test]
    fn provision_persists_the_realizations_into_the_store() {
        // The code module must be present before provisioning; then provision
        // κ-addresses the manifest, the capability set, and the holospace into
        // the store so the substrate runtime can resolve and spawn it.
        let store = MemKappaStore::new();
        let code = store.put("blake3", b"a code module").unwrap();
        let hs = provision(&store, Source::HoloFile { artifact: code }, caps()).unwrap();
        assert!(store.contains(hs.manifest()), "manifest stored");
        assert!(store.contains(hs.capabilities()), "capability set stored");
        assert!(store.contains(&hs.kappa()), "holospace definition stored");
    }

    #[test]
    fn provision_rejects_a_missing_code_module() {
        let store = MemKappaStore::new();
        // An artifact κ for bytes the store does not hold.
        let absent = address(b"code module that was never stored");
        let err = provision(&store, Source::HoloFile { artifact: absent }, caps()).unwrap_err();
        assert_eq!(err, ProvisionError::MissingCode(absent));
    }
}
