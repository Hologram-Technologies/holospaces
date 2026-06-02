//! **Configuration** — the control plane reconfigures a running holospace by
//! publishing content (ADR-018).
//!
//! holospaces is two things composed: the *control plane* (the operator's
//! Manager / control panel, ADR-001) and the *instances* (devcontainers). A
//! Codespaces/Gitpod control panel reconfigures a *running* environment across
//! four operation classes — **lifecycle, storage, network, account/user**.
//! holospaces does this UOR-native: when the operator configures an instance,
//! the panel produces a [`Configuration`] — a hologram [`Realization`] embedding
//! the issuing *operator* identity (Law L3) and the *target instance* κ, plus an
//! ordered set of [`Directive`]s. The control plane publishes it over the
//! substrate (content-addressed, like a roster); the running instance *resolves*
//! it (verify-by-re-derivation, Law L5), checks it targets *this* instance and
//! the operator is authorized, and *applies* it — its state changes. No
//! server, no control-plane→instance RPC: the configuration is content, the
//! substrate carries it (ADR-018; `CC-28`).
//!
//! A [`Configuration`] is itself a κ, so the same configuration applied to the
//! same instance yields the same resulting state (Law L1) — reconfiguration is
//! reproducible and auditable, and any of the operator's peers can issue it
//! (what-not-where).

use hologram_substrate_core::{Capabilities, Realization, RealizationError, References};

use crate::realizations::{address, encode, extract_refs, payload_of, Kappa};

#[cfg(not(feature = "std"))]
#[allow(unused_imports)]
use alloc::{vec, vec::Vec};

use core::fmt;

/// A lifecycle transition the control plane drives on an instance — the panel's
/// stop/suspend/resume/terminate controls (`CC-28`; the [`Session`] state
/// machine in [`crate::boot`] realizes the transition).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LifecycleAction {
    /// Boot a provisioned instance (panel "start").
    Start,
    /// Suspend a running instance to a κ snapshot (panel "suspend").
    Suspend,
    /// Resume a suspended instance from its snapshot (panel "resume").
    Resume,
    /// End an instance (panel "stop"/"terminate").
    Terminate,
}

impl LifecycleAction {
    fn tag(self) -> u8 {
        match self {
            LifecycleAction::Start => 0,
            LifecycleAction::Suspend => 1,
            LifecycleAction::Resume => 2,
            LifecycleAction::Terminate => 3,
        }
    }
    fn from_tag(t: u8) -> Option<Self> {
        Some(match t {
            0 => LifecycleAction::Start,
            1 => LifecycleAction::Suspend,
            2 => LifecycleAction::Resume,
            3 => LifecycleAction::Terminate,
            _ => return None,
        })
    }
}

/// One reconfiguration directive — a single control-panel operation, in one of
/// the four classes. A [`Configuration`] carries an ordered list of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Directive {
    /// **Lifecycle**: drive a state transition.
    Lifecycle(LifecycleAction),
    /// **Network**: forward a guest port out of the instance (app preview,
    /// `CC-21`) — the panel's Ports view.
    ForwardPort(u16),
    /// **Network**: stop forwarding a guest port.
    UnforwardPort(u16),
    /// **Network**: set the instance's outbound (`fetch`) and announce authority.
    SetNetwork {
        /// Whether the instance may make outbound network requests.
        fetch: bool,
        /// Whether the instance may announce content on the substrate.
        announce: bool,
    },
    /// **Storage**: set the instance's storage quota, in bytes.
    SetStorageQuota(u64),
    /// **Account/user**: grant another operator (by identity κ) the authority to
    /// reconfigure and resolve this instance — collaboration / sharing.
    GrantAccess(Kappa),
}

// Directive payload tags.
const D_LIFECYCLE: u8 = 1;
const D_FORWARD: u8 = 2;
const D_UNFORWARD: u8 = 3;
const D_NETWORK: u8 = 4;
const D_QUOTA: u8 = 5;
const D_GRANT: u8 = 6;

/// A **configuration** the control plane publishes to reconfigure a running
/// instance (ADR-018). A hologram [`Realization`]: its canonical form embeds the
/// issuing operator identity and the target instance κ (the two operands), and
/// carries the monotonic sequence number and the ordered directives as payload —
/// so the whole configuration is content (a κ). `seq` orders successive
/// configurations for one instance (the latest wins).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    operator: Kappa,
    instance: Kappa,
    seq: u64,
    directives: Vec<Directive>,
}

/// Why a configuration could not be applied (never silently ignored).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// The configuration targets a different instance than the one applying it.
    WrongInstance,
    /// The issuing operator is not authorized to reconfigure this instance.
    UnauthorizedOperator,
    /// The canonical bytes are not a well-formed configuration.
    Malformed(RealizationError),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::WrongInstance => f.write_str("configuration targets a different instance"),
            ConfigError::UnauthorizedOperator => {
                f.write_str("issuing operator is not authorized to reconfigure this instance")
            }
            ConfigError::Malformed(e) => write!(f, "malformed configuration: {e:?}"),
        }
    }
}

/// The result of applying a [`Configuration`] to an instance — the new effective
/// capability set plus the live effects the running machine must enact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// The instance's new effective capabilities (storage quota, network
    /// authority) — its identity changes with them (Law L1).
    pub capabilities: Capabilities,
    /// A lifecycle transition to drive, if the configuration requested one.
    pub lifecycle: Option<LifecycleAction>,
    /// Guest ports to begin forwarding (the panel's Ports view), in order.
    pub forward_ports: Vec<u16>,
    /// Guest ports to stop forwarding.
    pub unforward_ports: Vec<u16>,
    /// Operators granted reconfigure/resolve authority over the instance.
    pub grants: Vec<Kappa>,
}

impl Configuration {
    /// The holospaces realization IRI for a control-plane configuration.
    pub const IRI: &'static str = "https://uor.foundation/holospaces/realization/configuration";

    /// A configuration issued by `operator` for `instance`, at sequence `seq`,
    /// applying `directives` in order.
    #[must_use]
    pub fn new(operator: Kappa, instance: Kappa, seq: u64, directives: Vec<Directive>) -> Self {
        Self {
            operator,
            instance,
            seq,
            directives,
        }
    }

    /// The operator that issued this configuration.
    #[must_use]
    pub fn operator(&self) -> &Kappa {
        &self.operator
    }

    /// The instance this configuration targets.
    #[must_use]
    pub fn instance(&self) -> &Kappa {
        &self.instance
    }

    /// The monotonic sequence number (the latest configuration for an instance
    /// wins).
    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The ordered directives.
    #[must_use]
    pub fn directives(&self) -> &[Directive] {
        &self.directives
    }

    /// The configuration's κ — its content address (Law L1).
    #[must_use]
    pub fn kappa(&self) -> Kappa {
        address(&self.canonicalize())
    }

    /// Recover a configuration from its canonical form.
    ///
    /// # Errors
    ///
    /// [`RealizationError`] if the bytes are not a well-formed configuration.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, RealizationError> {
        let refs = <Self as Realization>::references(bytes)?;
        let (operator, rest) = refs.split_first().ok_or(RealizationError::Malformed)?;
        let instance = rest.first().ok_or(RealizationError::Malformed)?;
        let payload = payload_of(Self::IRI, bytes)?;
        let mut cur = 0usize;
        let seq = take_u64(&payload, &mut cur)?;
        let count = take_u32(&payload, &mut cur)? as usize;
        let mut directives = Vec::with_capacity(count);
        for _ in 0..count {
            directives.push(decode_directive(&payload, &mut cur)?);
        }
        Ok(Self {
            operator: *operator,
            instance: *instance,
            seq,
            directives,
        })
    }

    /// Apply this configuration to an instance: verify it targets `instance` and
    /// its operator is in `authorized`, then fold the directives into the new
    /// effective capabilities and the live effects. The caller (the
    /// [`Session`](crate::boot::Session) / the running machine) enacts the
    /// lifecycle transition and the port forwards.
    ///
    /// # Errors
    ///
    /// [`ConfigError::WrongInstance`] if it targets another instance;
    /// [`ConfigError::UnauthorizedOperator`] if the operator is not authorized.
    pub fn apply(
        &self,
        instance: &Kappa,
        authorized: &[Kappa],
        base: &Capabilities,
    ) -> Result<Applied, ConfigError> {
        if &self.instance != instance {
            return Err(ConfigError::WrongInstance);
        }
        if !authorized.contains(&self.operator) {
            return Err(ConfigError::UnauthorizedOperator);
        }
        let mut caps = base.clone();
        let mut applied = Applied {
            capabilities: base.clone(),
            lifecycle: None,
            forward_ports: Vec::new(),
            unforward_ports: Vec::new(),
            grants: Vec::new(),
        };
        for d in &self.directives {
            match d {
                Directive::Lifecycle(a) => applied.lifecycle = Some(*a),
                Directive::ForwardPort(p) => applied.forward_ports.push(*p),
                Directive::UnforwardPort(p) => applied.unforward_ports.push(*p),
                Directive::SetNetwork { fetch, announce } => {
                    caps.network_fetch = *fetch;
                    caps.network_announce = *announce;
                }
                Directive::SetStorageQuota(n) => caps.storage_quota_bytes = *n,
                Directive::GrantAccess(op) => applied.grants.push(*op),
            }
        }
        applied.capabilities = caps;
        Ok(applied)
    }

    /// The directives' payload region (after the operand κ-labels): `seq` then a
    /// length-framed list of encoded directives.
    fn payload(&self) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&self.seq.to_le_bytes());
        p.extend_from_slice(&(self.directives.len() as u32).to_le_bytes());
        for d in &self.directives {
            encode_directive(d, &mut p);
        }
        p
    }
}

impl Realization for Configuration {
    const IRI: hologram_substrate_core::RealizationId = Configuration::IRI;

    fn canonicalize(&self) -> Vec<u8> {
        encode(Self::IRI, &[self.operator, self.instance], &self.payload())
    }

    fn references(canonical_bytes: &[u8]) -> Result<References, RealizationError> {
        extract_refs(Self::IRI, canonical_bytes)
    }
}

fn encode_directive(d: &Directive, p: &mut Vec<u8>) {
    match d {
        Directive::Lifecycle(a) => {
            p.push(D_LIFECYCLE);
            p.push(a.tag());
        }
        Directive::ForwardPort(port) => {
            p.push(D_FORWARD);
            p.extend_from_slice(&port.to_le_bytes());
        }
        Directive::UnforwardPort(port) => {
            p.push(D_UNFORWARD);
            p.extend_from_slice(&port.to_le_bytes());
        }
        Directive::SetNetwork { fetch, announce } => {
            p.push(D_NETWORK);
            p.push(u8::from(*fetch));
            p.push(u8::from(*announce));
        }
        Directive::SetStorageQuota(n) => {
            p.push(D_QUOTA);
            p.extend_from_slice(&n.to_le_bytes());
        }
        Directive::GrantAccess(op) => {
            p.push(D_GRANT);
            p.extend_from_slice(op.as_array());
        }
    }
}

fn decode_directive(p: &[u8], cur: &mut usize) -> Result<Directive, RealizationError> {
    let tag = take_u8(p, cur)?;
    Ok(match tag {
        D_LIFECYCLE => {
            let a =
                LifecycleAction::from_tag(take_u8(p, cur)?).ok_or(RealizationError::Malformed)?;
            Directive::Lifecycle(a)
        }
        D_FORWARD => Directive::ForwardPort(take_u16(p, cur)?),
        D_UNFORWARD => Directive::UnforwardPort(take_u16(p, cur)?),
        D_NETWORK => {
            let fetch = take_u8(p, cur)? != 0;
            let announce = take_u8(p, cur)? != 0;
            Directive::SetNetwork { fetch, announce }
        }
        D_QUOTA => Directive::SetStorageQuota(take_u64(p, cur)?),
        D_GRANT => Directive::GrantAccess(take_kappa(p, cur)?),
        _ => return Err(RealizationError::Malformed),
    })
}

fn take_u8(p: &[u8], cur: &mut usize) -> Result<u8, RealizationError> {
    let v = *p.get(*cur).ok_or(RealizationError::Truncated)?;
    *cur += 1;
    Ok(v)
}
fn take_u16(p: &[u8], cur: &mut usize) -> Result<u16, RealizationError> {
    let end = cur.checked_add(2).ok_or(RealizationError::Truncated)?;
    let arr: [u8; 2] = p
        .get(*cur..end)
        .ok_or(RealizationError::Truncated)?
        .try_into()
        .map_err(|_| RealizationError::Truncated)?;
    *cur = end;
    Ok(u16::from_le_bytes(arr))
}
fn take_u32(p: &[u8], cur: &mut usize) -> Result<u32, RealizationError> {
    let end = cur.checked_add(4).ok_or(RealizationError::Truncated)?;
    let arr: [u8; 4] = p
        .get(*cur..end)
        .ok_or(RealizationError::Truncated)?
        .try_into()
        .map_err(|_| RealizationError::Truncated)?;
    *cur = end;
    Ok(u32::from_le_bytes(arr))
}
fn take_u64(p: &[u8], cur: &mut usize) -> Result<u64, RealizationError> {
    let end = cur.checked_add(8).ok_or(RealizationError::Truncated)?;
    let arr: [u8; 8] = p
        .get(*cur..end)
        .ok_or(RealizationError::Truncated)?
        .try_into()
        .map_err(|_| RealizationError::Truncated)?;
    *cur = end;
    Ok(u64::from_le_bytes(arr))
}
fn take_kappa(p: &[u8], cur: &mut usize) -> Result<Kappa, RealizationError> {
    const KAPPA71: usize = 71;
    let end = cur
        .checked_add(KAPPA71)
        .ok_or(RealizationError::Truncated)?;
    let arr: [u8; KAPPA71] = p
        .get(*cur..end)
        .ok_or(RealizationError::Truncated)?
        .try_into()
        .map_err(|_| RealizationError::Truncated)?;
    *cur = end;
    Kappa::from_bytes(&arr).map_err(|_| RealizationError::Malformed)
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

    fn config() -> Configuration {
        Configuration::new(
            address(b"operator-key"),
            address(b"instance-kappa"),
            7,
            vec![
                Directive::Lifecycle(LifecycleAction::Resume),
                Directive::ForwardPort(8080),
                Directive::SetNetwork {
                    fetch: true,
                    announce: false,
                },
                Directive::SetStorageQuota(1 << 30),
                Directive::GrantAccess(address(b"collaborator")),
            ],
        )
    }

    #[test]
    fn round_trips_through_its_canonical_form() {
        let c = config();
        let bytes = c.canonicalize();
        let back = Configuration::from_canonical(&bytes).expect("decode");
        assert_eq!(back, c);
        assert_eq!(back.operator(), c.operator());
        assert_eq!(back.instance(), c.instance());
        assert_eq!(back.seq(), 7);
        assert_eq!(back.directives(), c.directives());
    }

    #[test]
    fn is_reproducible_content() {
        // QS1: the same configuration yields the same κ on any peer.
        assert_eq!(config().kappa(), config().kappa());
    }

    #[test]
    fn a_different_operator_or_directive_is_a_different_kappa() {
        let base = config().kappa();
        let other_op = Configuration::new(
            address(b"different-operator"),
            address(b"instance-kappa"),
            7,
            config().directives().to_vec(),
        );
        assert_ne!(
            base,
            other_op.kappa(),
            "the operator is part of identity (L3)"
        );
        let other_dir = Configuration::new(
            address(b"operator-key"),
            address(b"instance-kappa"),
            7,
            vec![Directive::ForwardPort(9090)],
        );
        assert_ne!(base, other_dir.kappa());
    }

    #[test]
    fn apply_folds_directives_into_caps_and_effects() {
        let c = config();
        let owner = address(b"operator-key");
        let applied = c
            .apply(&address(b"instance-kappa"), &[owner], &caps())
            .expect("authorized + right instance");
        assert_eq!(applied.lifecycle, Some(LifecycleAction::Resume));
        assert_eq!(applied.forward_ports, vec![8080]);
        assert!(applied.capabilities.network_fetch);
        assert!(!applied.capabilities.network_announce);
        assert_eq!(applied.capabilities.storage_quota_bytes, 1 << 30);
        assert_eq!(applied.grants, vec![address(b"collaborator")]);
    }

    #[test]
    fn a_configuration_for_another_instance_is_refused() {
        let c = config();
        let err = c
            .apply(
                &address(b"some-other-instance"),
                &[address(b"operator-key")],
                &caps(),
            )
            .unwrap_err();
        assert_eq!(err, ConfigError::WrongInstance);
    }

    #[test]
    fn an_unauthorized_operator_is_refused() {
        let c = config();
        // The owner is someone else; the issuing operator is not authorized.
        let err = c
            .apply(
                &address(b"instance-kappa"),
                &[address(b"the-real-owner")],
                &caps(),
            )
            .unwrap_err();
        assert_eq!(err, ConfigError::UnauthorizedOperator);
    }
}
