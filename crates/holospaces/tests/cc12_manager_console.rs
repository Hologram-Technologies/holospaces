//! `CC-12` — the Platform Manager management console (arc42 ch.10; ADR-010).
//!
//! The console signs an operator in to a **self-sovereign, content-addressed
//! identity** (`CC-1`) and provisions holospaces from validated devcontainers
//! (`CC-4`) whose identity is reproducible (Law L1). The browser dashboard
//! (`crates/holospaces-web/web/manager-test.mjs`) witnesses the full surface;
//! these witness the invariants the console composes.

use holospaces::address;
use holospaces::boot::devcontainer;
use holospaces::identity::Operator;

/// Sign-in is a self-sovereign identity — content, not a server account (`CC-1`):
/// the same key always yields the same identity κ (Law L1), different keys differ.
#[test]
fn the_console_signs_in_a_self_sovereign_content_addressed_identity() {
    let operator = Operator::from_public_key(b"operator-self-sovereign-key");
    assert!(
        operator.identity().as_str().starts_with("blake3:"),
        "the operator identity is content-addressed, not a server account"
    );
    let again = Operator::from_public_key(b"operator-self-sovereign-key");
    assert_eq!(
        operator.identity(),
        again.identity(),
        "same key ⇒ same identity (L1)"
    );
    let other = Operator::from_public_key(b"a-different-operator");
    assert_ne!(operator.identity(), other.identity());
}

/// Creating a holospace validates its `devcontainer.json` against the Dev
/// Container spec (`CC-4`) and addresses it by content — the holospace identity
/// is reproducible (same definition ⇒ same κ, Law L1 / Q4); an invalid config is
/// refused.
#[test]
fn the_console_provisions_a_validated_reproducible_devcontainer() {
    let config = br#"{"name":"my-devcontainer","image":"debian:12","features":{}}"#;
    devcontainer::parse(config).expect("a valid devcontainer.json (CC-4)");

    let first = address(config);
    let second = address(config);
    assert_eq!(
        first, second,
        "same devcontainer definition ⇒ same holospace κ (L1 / Q4)"
    );
    assert!(first.as_str().starts_with("blake3:"));

    assert!(
        devcontainer::parse(b"not a devcontainer").is_err(),
        "an invalid devcontainer.json is refused (CC-4)"
    );
}
