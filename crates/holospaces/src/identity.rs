//! **Identity** — self-sovereign sign-in, and the keying that lets an
//! operator's instances sync.
//!
//! Realizes the *Identity* building block (arc42 chapter 5,
//! `docs/src/arc42/adoc/05_building_block_view.adoc`) and the cross-cutting
//! concept *Identity and sync* (arc42 chapter 8).
//!
//! The operator signs in by unlocking a *self-sovereign key* — not a server
//! account (there is no server, ADR-001). That identity is content: the
//! [`Operator`] identity is the [`Kappa`] of the operator's public key, the
//! same on every instance, which is how an operator's instances recognise one
//! another and scope what they announce and resolve over the substrate's
//! `KappaSync`.
//!
//! The key material, its generation, and the unlock are the substrate's
//! keystore, consumed by reference (ADR-006); holospaces holds only the
//! κ-addressed identity (Law L3).

use hologram_substrate_core::{Realization, RealizationError, References};

use crate::realizations::{address, encode, extract_refs, Kappa};

/// An operator, identified by the κ of their self-sovereign public key.
///
/// Two instances that unlock the same key compute the same [`Operator`]
/// identity, linking them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operator {
    identity: Kappa,
}

impl Operator {
    /// The operator whose identity is `identity`.
    #[must_use]
    pub fn new(identity: Kappa) -> Self {
        Self { identity }
    }

    /// Derive an operator's identity from their self-sovereign public key
    /// bytes — the κ-label of the key (Law L1).
    #[must_use]
    pub fn from_public_key(public_key: &[u8]) -> Self {
        Self {
            identity: address(public_key),
        }
    }

    /// The operator's κ-addressed identity.
    #[must_use]
    pub fn identity(&self) -> &Kappa {
        &self.identity
    }
}

/// A **roster** — the set of holospaces an operator owns, scoped to their
/// identity (arc42 chapter 8, *Identity and sync*; R5). It is a hologram
/// [`Realization`]: IRI-tagged canonical bytes embedding the operator identity
/// and the operator's holospace κ-labels, so the whole roster is itself
/// content (a κ). An operator's instances synchronise by resolving the roster
/// κ over the substrate and then resolving each holospace it lists — all
/// verified by re-derivation (Law L5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Roster {
    operator: Kappa,
    holospaces: Vec<Kappa>,
}

impl Roster {
    /// The holospaces realization IRI for an operator roster.
    pub const IRI: &'static str = "https://uor.foundation/holospaces/realization/roster";

    /// A roster of `holospaces` owned by `operator`.
    #[must_use]
    pub fn new(operator: &Operator, holospaces: Vec<Kappa>) -> Self {
        Self {
            operator: *operator.identity(),
            holospaces,
        }
    }

    /// The operator identity this roster is scoped to.
    #[must_use]
    pub fn operator(&self) -> &Kappa {
        &self.operator
    }

    /// The operator's holospace κ-labels.
    #[must_use]
    pub fn holospaces(&self) -> &[Kappa] {
        &self.holospaces
    }

    /// The roster's κ — its content address (Law L1).
    #[must_use]
    pub fn kappa(&self) -> Kappa {
        address(&self.canonicalize())
    }

    /// Recover a roster from its canonical form (the operator is the first
    /// embedded operand; the rest are its holospaces).
    ///
    /// # Errors
    ///
    /// [`RealizationError`] if the bytes are not a well-formed roster.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, RealizationError> {
        let refs = <Self as Realization>::references(bytes)?;
        let (operator, holospaces) = refs.split_first().ok_or(RealizationError::Malformed)?;
        Ok(Self {
            operator: *operator,
            holospaces: holospaces.to_vec(),
        })
    }
}

impl Realization for Roster {
    const IRI: hologram_substrate_core::RealizationId = Roster::IRI;

    fn canonicalize(&self) -> Vec<u8> {
        let mut refs = Vec::with_capacity(1 + self.holospaces.len());
        refs.push(self.operator);
        refs.extend_from_slice(&self.holospaces);
        encode(Self::IRI, &refs, &[])
    }

    fn references(canonical_bytes: &[u8]) -> Result<References, RealizationError> {
        extract_refs(Self::IRI, canonical_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_key_yields_same_identity_across_instances() {
        let key = b"ed25519-public-key-bytes";
        let here = Operator::from_public_key(key);
        let there = Operator::from_public_key(key);
        assert_eq!(here, there);
        assert_eq!(here.identity().sigma_axis(), Some("blake3"));
    }

    #[test]
    fn distinct_keys_yield_distinct_identities() {
        assert_ne!(
            Operator::from_public_key(b"key-a"),
            Operator::from_public_key(b"key-b")
        );
    }

    #[test]
    fn roster_round_trips_through_its_canonical_form() {
        let operator = Operator::from_public_key(b"operator-key");
        let holospaces = vec![address(b"holospace-a"), address(b"holospace-b")];
        let roster = Roster::new(&operator, holospaces.clone());

        let bytes = roster.canonicalize();
        let back = Roster::from_canonical(&bytes).expect("decode roster");
        assert_eq!(back, roster);
        assert_eq!(back.operator(), operator.identity());
        assert_eq!(back.holospaces(), holospaces.as_slice());
        // The roster is itself content — a stable κ links the operator's instances.
        assert_eq!(roster.kappa(), Roster::new(&operator, holospaces).kappa());
    }
}
