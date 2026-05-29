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

use crate::realizations::{address, Kappa};

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
}
