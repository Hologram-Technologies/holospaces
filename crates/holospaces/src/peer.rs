//! **Peer** — an environment that *becomes* the substrate (arc42 chapter 7,
//! `docs/src/arc42/adoc/07_deployment_view.adoc`).
//!
//! A peer composes the [hologram](https://github.com/Hologram-Technologies/hologram)
//! substrate pillars for one environment — storage (`KappaStore`), network
//! (`KappaSync`), and runtime (`ContainerRuntime`) — and supplies the boot
//! operations over them. It does not connect to a server (Law L1); a single
//! online peer is the one-participant case of the content-addressed mesh.
//!
//! [`Peer`] is environment-agnostic (generic over the substrate it composes).
//! The *native* peer composes hologram's native store + a `Runtime` over the
//! Wasmtime engine; the *browser* and *bare-metal* peers compose hologram's
//! OPFS / bare-metal backends and the executor compiled to Wasm / no-OS. The
//! same holospace κ boots on any peer (quality goal Q6).

use hologram_substrate_core::{
    get_with_fetch, AccessError, Bytes, Capabilities, ContainerRuntime, KappaStore, KappaSync,
};

use crate::boot::{provision, ProvisionError, Resolver, Session};
use crate::realizations::{Holospace, Kappa, Source};

/// A holospaces peer: the composed substrate plus the boot operations over it.
///
/// Borrows the substrate pillars it composes — the store, the runtime, and
/// (when online) the network — so the same `Peer` API drives any environment's
/// concrete backends.
pub struct Peer<'a, R: ContainerRuntime> {
    store: &'a dyn KappaStore,
    runtime: &'a R,
    sync: Option<&'a dyn KappaSync>,
}

impl<'a, R: ContainerRuntime> Peer<'a, R> {
    /// Compose a peer from a store and a runtime (offline — a one-participant
    /// mesh).
    pub fn new(store: &'a dyn KappaStore, runtime: &'a R) -> Self {
        Self {
            store,
            runtime,
            sync: None,
        }
    }

    /// Add the network pillar so the peer can fetch content from other peers
    /// (verify-on-receipt, Law L5).
    #[must_use]
    pub fn with_sync(mut self, sync: &'a dyn KappaSync) -> Self {
        self.sync = Some(sync);
        self
    }

    /// The peer's content-addressed store.
    pub fn store(&self) -> &dyn KappaStore {
        self.store
    }

    /// The peer's substrate runtime.
    pub fn runtime(&self) -> &R {
        self.runtime
    }

    /// Provision a holospace into this peer's store (Law L2) — see
    /// [`crate::boot::provision`].
    ///
    /// # Errors
    ///
    /// [`ProvisionError`] if a part cannot be stored or the code is absent.
    pub fn provision(
        &self,
        source: Source,
        capabilities: Capabilities,
    ) -> Result<Holospace, ProvisionError> {
        provision(self.store, source, capabilities)
    }

    /// Resolve a κ-label to its verified bytes: local store first, else fetch
    /// over the network and verify on receipt (Law L5; the substrate's
    /// eviction-tolerant read). With no network pillar, resolution is local.
    ///
    /// # Errors
    ///
    /// [`AccessError`] on a store/sync failure or a re-derivation mismatch.
    pub async fn resolve(&self, kappa: &Kappa) -> Result<Option<Bytes>, AccessError> {
        match self.sync {
            Some(sync) => get_with_fetch(self.store, sync, kappa).await,
            None => Resolver::resolve_local(self.store, kappa),
        }
    }

    /// Resolve the transitive *reachability closure* of `root` into this peer
    /// (SPINE-3): the root and every operand κ it embeds — for a holospace, its
    /// manifest, capability set, and code module — each fetched and verified on
    /// receipt (Law L5). This is what migrates a holospace to another peer so
    /// the runtime can spawn it. Refs that resolve to nothing (e.g. an empty
    /// initial-state, or the operator identity) are tolerated. Returns the
    /// number of resolved nodes.
    ///
    /// # Errors
    ///
    /// [`AccessError`] on a store/sync failure or a re-derivation mismatch.
    pub async fn resolve_closure(&self, root: &Kappa) -> Result<usize, AccessError> {
        let registry = crate::realizations::registry();
        let mut stack = vec![*root];
        let mut seen = std::collections::HashSet::new();
        let mut resolved = 0usize;
        while let Some(k) = stack.pop() {
            if !seen.insert(k.as_str().to_owned()) {
                continue;
            }
            if let Some(bytes) = self.resolve(&k).await? {
                resolved += 1;
                // Leaf content (code modules, snapshots) carries no realization
                // IRI; `references` returns an error there — a leaf, not a fault.
                if let Ok(refs) = hologram_substrate_core::references(&bytes, &registry) {
                    stack.extend(refs);
                }
            }
        }
        Ok(resolved)
    }

    /// Begin a lifecycle [`Session`] for a holospace on this peer's runtime.
    pub fn session(&self, holospace: Holospace) -> Session<'_, R> {
        Session::provision(self.runtime, holospace)
    }
}
