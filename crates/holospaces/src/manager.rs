//! **Platform Manager** — the operator console's model.
//!
//! Realizes the *Platform Manager* building block (arc42 chapter 5,
//! `docs/src/arc42/adoc/05_building_block_view.adoc`) and the operator
//! relationship of the context view (arc42 chapter 3): a familiar
//! virtualization / container-management console. The Manager is "the first-
//! party holospace: the operator console that provisions and manages
//! holospaces" — itself served from GitHub Pages.
//!
//! This module is the Manager's **model**: a [`View`] (the projection of the
//! operator's holospaces) and the *Intent* surface (provision / open a
//! lifecycle [`Session`] / synchronise). A rendered
//! console (browser / native) is a thin presentation over this model. The
//! Manager's own state — the operator's [`Roster`] — is canonical and held in
//! the peer's store (Law L2), so it synchronises across the operator's
//! instances over the substrate (R5).

use hologram_realizations::CapabilitySet;
use hologram_substrate_core::{
    AccessError, Capabilities, ContainerRuntime, Realization, RealizationError,
};

use crate::boot::{ProvisionError, ReconfigureError, Session};
use crate::config::{Configuration, Directive};
use crate::identity::{Operator, Roster};
use crate::peer::Peer;
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

/// A projection of the operator's holospaces — what the console renders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The signed-in operator's identity κ.
    pub operator: Kappa,
    /// The κ-labels of the operator's holospaces.
    pub holospaces: Vec<Kappa>,
}

/// The Platform Manager over a [`Peer`] for a signed-in [`Operator`].
pub struct Manager<'a, R: ContainerRuntime> {
    peer: Peer<'a, R>,
    operator: Operator,
    holospaces: Vec<Kappa>,
    /// The latest configuration published per instance — `(instance κ, config κ,
    /// next seq)`. The control plane's record of what it has reconfigured each
    /// instance to (ADR-018).
    configs: Vec<(Kappa, Kappa, u64)>,
    /// Operators granted reconfigure/resolve authority per instance — the
    /// account/user management the control plane tracks (the owner is always
    /// authorized; grants extend the set).
    grants: Vec<(Kappa, Vec<Kappa>)>,
}

impl<'a, R: ContainerRuntime> Manager<'a, R> {
    /// Sign in: bind the console to a peer and a self-sovereign operator
    /// identity (no server account — ADR-001).
    pub fn sign_in(peer: Peer<'a, R>, operator: Operator) -> Self {
        Self {
            peer,
            operator,
            holospaces: Vec::new(),
            configs: Vec::new(),
            grants: Vec::new(),
        }
    }

    /// The signed-in operator.
    pub fn operator(&self) -> &Operator {
        &self.operator
    }

    /// The current projection (the View the console renders).
    pub fn view(&self) -> View {
        View {
            operator: *self.operator.identity(),
            holospaces: self.holospaces.clone(),
        }
    }

    /// The operator's roster as a canonical [`Roster`] (its κ links the
    /// operator's instances; R5).
    pub fn roster(&self) -> Roster {
        Roster::new(&self.operator, self.holospaces.clone())
    }

    /// *Intent: provision.* Provision a holospace into the peer's store and add
    /// it to the operator's roster (the updated roster is itself stored, so it
    /// is content-addressed and syncable).
    ///
    /// # Errors
    ///
    /// [`ManagerError`] if provisioning or persisting the roster fails.
    pub fn provision(
        &mut self,
        source: Source,
        capabilities: Capabilities,
    ) -> Result<Kappa, ManagerError> {
        let holospace = self.peer.provision(source, capabilities)?;
        let kappa = holospace.kappa();
        if !self.holospaces.contains(&kappa) {
            self.holospaces.push(kappa);
        }
        self.persist_roster()?;
        Ok(kappa)
    }

    /// *Intent: open.* Resolve a holospace by κ (fetch + verify, Law L5) and
    /// open a lifecycle [`Session`] for it on the peer's runtime. The caller
    /// drives `boot` / `suspend` / `resume` / `terminate`.
    ///
    /// # Errors
    ///
    /// [`ManagerError::NotFound`] if the holospace cannot be resolved, or a
    /// resolution / decoding error.
    pub async fn open(&self, holospace: &Kappa) -> Result<Session<'a, R>, ManagerError> {
        let bytes = self
            .peer
            .resolve(holospace)
            .await?
            .ok_or(ManagerError::NotFound)?;
        let definition = Holospace::from_canonical(&bytes)?;
        Ok(self.peer.session(definition))
    }

    /// *Intent: synchronise.* Adopt another of the operator's instances by
    /// resolving its [`Roster`] κ over the substrate (verify-on-receipt, Law
    /// L5) and resolving every holospace it lists into this peer. Returns how
    /// many holospaces were synchronised. This is how an operator signs in on a
    /// new instance and finds their holospaces (R5 / QS5).
    ///
    /// # Errors
    ///
    /// [`ManagerError`] if the roster or a holospace cannot be resolved.
    pub async fn sync_from(&mut self, roster: &Kappa) -> Result<usize, ManagerError> {
        let bytes = self
            .peer
            .resolve(roster)
            .await?
            .ok_or(ManagerError::NotFound)?;
        let roster = Roster::from_canonical(&bytes)?;
        let mut synced = 0usize;
        for holospace in roster.holospaces() {
            // Resolve the holospace's whole reachability closure (manifest,
            // capability set, code) so this peer can boot it (SPINE-3, L5).
            if self.peer.resolve_closure(holospace).await? > 0 {
                if !self.holospaces.contains(holospace) {
                    self.holospaces.push(*holospace);
                }
                synced += 1;
            }
        }
        self.persist_roster()?;
        Ok(synced)
    }

    /// *Intent: configure.* Reconfigure a running instance from the control
    /// panel (ADR-018). Builds a [`Configuration`] issued by this operator for
    /// `instance` (the next sequence number), stores it as content (Law L2), and
    /// records it as the instance's current configuration. Returns the
    /// configuration κ — the content the instance resolves and applies over the
    /// substrate (no server, no RPC; `CC-28`). The directives span the four
    /// operation classes: lifecycle / storage / network / account-user.
    ///
    /// # Errors
    ///
    /// [`ManagerError::Store`] if the configuration cannot be persisted.
    pub fn configure(
        &mut self,
        instance: &Kappa,
        directives: Vec<Directive>,
    ) -> Result<Kappa, ManagerError> {
        let seq = match self.configs.iter().find(|(i, _, _)| i == instance) {
            Some((_, _, next)) => *next,
            None => 0,
        };
        let config = Configuration::new(*self.operator.identity(), *instance, seq, directives);
        let kappa = config.kappa();
        self.peer
            .store()
            .put("blake3", &config.canonicalize())
            .map_err(|e| ManagerError::Store(format!("{e:?}")))?;
        match self.configs.iter_mut().find(|(i, _, _)| i == instance) {
            Some(entry) => {
                entry.1 = kappa;
                entry.2 = seq + 1;
            }
            None => self.configs.push((*instance, kappa, seq + 1)),
        }
        Ok(kappa)
    }

    /// The κ of the latest configuration this control plane published for
    /// `instance`, if any — what the instance resolves to reconfigure itself.
    #[must_use]
    pub fn configuration_of(&self, instance: &Kappa) -> Option<Kappa> {
        self.configs
            .iter()
            .find(|(i, _, _)| i == instance)
            .map(|(_, k, _)| *k)
    }

    /// *Intent: resolve a configuration.* Resolve a [`Configuration`] κ over the
    /// substrate (fetch + verify-by-re-derivation, Law L5) and decode it — the
    /// instance side of reconfiguration: it picks up the control plane's
    /// published configuration as *content* and is then applied via
    /// [`Configuration::apply`]. Mirrors [`Self::open`] (`CC-28`, ADR-018).
    ///
    /// # Errors
    ///
    /// [`ManagerError::NotFound`] if the configuration cannot be resolved, or a
    /// resolution / decode error.
    pub async fn resolve_configuration(
        &self,
        config: &Kappa,
    ) -> Result<Configuration, ManagerError> {
        let bytes = self
            .peer
            .resolve(config)
            .await?
            .ok_or(ManagerError::NotFound)?;
        Ok(Configuration::from_canonical(&bytes)?)
    }

    /// *Intent: manage.* **Actually reconfigure a running instance** the control
    /// plane manages (ADR-018; `CC-28`). Publishes the configuration as content
    /// (so any peer can resolve it) *and* drives `session` with it: it resolves
    /// the instance's current effective capabilities, authorizes the operator
    /// (the owner plus any tracked grants), and applies the directives —
    /// **driving the real lifecycle transition** (start/suspend/resume/terminate)
    /// and replacing the effective capability set. The instance's state actually
    /// changes; the panel is not just publishing intent. Returns the published
    /// configuration κ. The returned [`Applied`]'s grants are recorded so a
    /// granted collaborator can manage the instance thereafter.
    ///
    /// # Errors
    ///
    /// [`ManagerError`] if the configuration cannot be persisted, the current
    /// capabilities cannot be resolved, or the reconfiguration is not applicable.
    pub async fn reconfigure(
        &mut self,
        session: &mut Session<'a, R>,
        directives: Vec<Directive>,
    ) -> Result<Kappa, ManagerError> {
        let instance = session.holospace().kappa();
        let config_kappa = self.configure(&instance, directives)?;
        let config = self.resolve_configuration(&config_kappa).await?;

        // The instance's current effective capabilities (decoded from its κ) — the
        // base the directives fold over (so a quota change does not reset network).
        let caps_bytes = self
            .peer
            .resolve(session.holospace().capabilities())
            .await?
            .ok_or(ManagerError::NotFound)?;
        let base = CapabilitySet::to_capabilities(&caps_bytes)?;

        // Authorized: the owner (this operator) plus any tracked grants.
        let mut authorized = vec![*self.operator.identity()];
        if let Some((_, g)) = self.grants.iter().find(|(i, _)| *i == instance) {
            authorized.extend_from_slice(g);
        }

        let applied = session
            .reconfigure(&config, &authorized, &base)
            .await
            .map_err(ManagerError::Reconfigure)?;

        // A capability change makes the instance a new effective state (a new κ,
        // Law L1); persist its new capability set + holospace so the next
        // reconfiguration resolves them (Law L2), and follow the instance's
        // identity in the roster.
        let new_instance = session.holospace().kappa();
        if new_instance != instance {
            let store = self.peer.store();
            store
                .put(
                    "blake3",
                    &CapabilitySet::new(applied.capabilities.clone()).canonicalize(),
                )
                .map_err(|e| ManagerError::Store(format!("{e:?}")))?;
            store
                .put("blake3", &session.holospace().canonicalize())
                .map_err(|e| ManagerError::Store(format!("{e:?}")))?;
            if let Some(slot) = self.holospaces.iter_mut().find(|k| **k == instance) {
                *slot = new_instance;
            }
            // Carry the per-instance config/grant records onto the new identity.
            for (i, _, _) in self.configs.iter_mut().filter(|(i, _, _)| *i == instance) {
                *i = new_instance;
            }
            for (i, _) in self.grants.iter_mut().filter(|(i, _)| *i == instance) {
                *i = new_instance;
            }
        }

        // Record account/user grants so the granted operators may manage it next.
        if !applied.grants.is_empty() {
            match self.grants.iter_mut().find(|(i, _)| *i == instance) {
                Some((_, g)) => {
                    for op in &applied.grants {
                        if !g.contains(op) {
                            g.push(*op);
                        }
                    }
                }
                None => self.grants.push((instance, applied.grants.clone())),
            }
        }
        Ok(config_kappa)
    }

    fn persist_roster(&self) -> Result<(), ManagerError> {
        self.peer
            .store()
            .put("blake3", &self.roster().canonicalize())
            .map_err(|e| ManagerError::Store(format!("{e:?}")))?;
        Ok(())
    }
}

/// Why a Manager intent failed.
#[derive(Debug)]
pub enum ManagerError {
    /// A referenced holospace or roster could not be resolved.
    NotFound,
    /// Provisioning into the store failed.
    Provision(ProvisionError),
    /// Resolving content over the substrate failed.
    Access(AccessError),
    /// A canonical form could not be decoded.
    Realization(RealizationError),
    /// Reconfiguring a managed instance failed (wrong target / unauthorized /
    /// a driven lifecycle transition failed).
    Reconfigure(ReconfigureError),
    /// Persisting the roster failed.
    Store(String),
}

impl core::fmt::Display for ManagerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ManagerError::NotFound => f.write_str("holospace or roster could not be resolved"),
            ManagerError::Provision(e) => write!(f, "provision failed: {e}"),
            ManagerError::Access(e) => write!(f, "substrate access failed: {e:?}"),
            ManagerError::Realization(e) => write!(f, "canonical form decode failed: {e:?}"),
            ManagerError::Reconfigure(e) => write!(f, "reconfigure failed: {e}"),
            ManagerError::Store(e) => write!(f, "roster persistence failed: {e}"),
        }
    }
}

impl core::error::Error for ManagerError {}

impl From<ProvisionError> for ManagerError {
    fn from(e: ProvisionError) -> Self {
        ManagerError::Provision(e)
    }
}
impl From<AccessError> for ManagerError {
    fn from(e: AccessError) -> Self {
        ManagerError::Access(e)
    }
}
impl From<RealizationError> for ManagerError {
    fn from(e: RealizationError) -> Self {
        ManagerError::Realization(e)
    }
}
