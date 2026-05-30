//! **CC-4 — a devcontainer holospace matches its source.**
//!
//! The Conformance catalog row `CC-4` (arc42 chapter 10,
//! `docs/src/arc42/adoc/10_quality_requirements.adoc`): a devcontainer
//! holospace matches its source, validated against the
//! [Dev Container](https://containers.dev) and OCI image specifications, by a
//! reproducible-κ check (Q4).
//!
//! The external authority is the **published Dev Container JSON Schema**
//! (`devContainer.base.schema.json`, imported verbatim from `devcontainers/spec`)
//! and **real authoritative `devcontainer.json` configs** (imported verbatim
//! from `devcontainers/templates` plus this repository's own), pinned in
//! `vv/artifacts/cc4/` (provenance in `vv/PROVENANCE.md`). No hand-authored
//! cases: the schema is the judge of conformance.
//!
//! This witness checks:
//! 1. **Conformance** — every real config validates against the schema, and
//!    holospaces' ingestor ([`holospaces::boot::devcontainer::parse`]) accepts
//!    it; a config the schema rejects is rejected.
//! 2. **Matches its source** — ingesting the same config yields the same
//!    holospace κ; a different config yields a different κ (QS1 / Q4).
//!
//! Run by `vv/run.sh`; also `cargo test -p holospaces --test cc4_devcontainer`.

use holospaces::boot::devcontainer::{self, to_canonical_json};
use holospaces::boot::ingest_devcontainer;
use holospaces::Capabilities;
use serde_json::Value;

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

fn artifact(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vv/artifacts/cc4")
        .join(name)
}

fn schema() -> jsonschema::Validator {
    let raw = std::fs::read(artifact("devContainer.base.schema.json")).expect("read schema");
    let doc: Value = serde_json::from_slice(&raw).expect("schema is JSON");
    jsonschema::validator_for(&doc).expect("compile Dev Container schema")
}

/// Real authoritative configs imported verbatim from `devcontainers/templates`
/// — each declares an OCI image source, so the Dev Container *base* schema
/// covers them.
fn template_configs() -> Vec<(String, Vec<u8>)> {
    ["rust", "javascript-node", "python", "ubuntu"]
        .iter()
        .map(|t| {
            (
                (*t).to_owned(),
                std::fs::read(artifact(&format!("{t}.devcontainer.json"))).unwrap(),
            )
        })
        .collect()
}

fn repo_config() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.devcontainer/devcontainer.json"),
    )
    .expect("read repo devcontainer.json")
}

/// (1) Every real authoritative template config validates against the
/// authoritative Dev Container schema and is accepted by holospaces' ingestor.
#[test]
fn real_configs_conform_to_the_dev_container_schema() {
    let schema = schema();
    for (name, raw) in template_configs() {
        let json = to_canonical_json(&raw).unwrap_or_else(|e| panic!("{name}: JSONC→JSON: {e}"));
        let value: Value = serde_json::from_slice(&json).unwrap();
        assert!(
            schema.is_valid(&value),
            "{name} does not validate against the Dev Container schema"
        );
        devcontainer::parse(&raw).unwrap_or_else(|e| panic!("{name}: ingestor rejected: {e}"));
    }
}

/// This repository's own `.devcontainer/devcontainer.json` is a *features-only*
/// configuration: it declares no image source and relies on the implementor's
/// default base image (the Codespaces behavior). The Dev Container *base*
/// schema requires an explicit image source, so it does not model this case;
/// holospaces' ingestor accepts it as the documented default-image superset.
#[test]
fn repo_features_only_config_ingests_as_default_image() {
    let dc = devcontainer::parse(&repo_config()).expect("ingestor accepts the repo config");
    assert_eq!(dc.image_source, devcontainer::ImageSource::Default);
}

/// (1, negative) A config the authoritative schema rejects is rejected — the
/// schema is the judge, not a hand-written verdict.
#[test]
fn schema_rejects_a_nonconformant_config() {
    let schema = schema();
    // `forwardPorts` must be an array per the schema; a string violates it.
    let bad: Value = serde_json::json!({ "image": "debian:12", "forwardPorts": "3000" });
    assert!(
        !schema.is_valid(&bad),
        "schema must reject a malformed config"
    );
}

/// (2) A devcontainer holospace matches its source: same config ⇒ same κ;
/// different config ⇒ different κ (QS1 / Q4).
#[test]
fn devcontainer_holospace_is_reproducible_from_its_source() {
    let configs = template_configs();
    let cfg = &configs.iter().find(|(n, _)| n == "rust").unwrap().1;
    let other = &configs.iter().find(|(n, _)| n == "python").unwrap().1;
    let userland = holospaces::address(b"the userland these devcontainers select");
    let mk = |c: &[u8]| {
        ingest_devcontainer(
            "https://x.invalid/r.git",
            "main",
            ".devcontainer/devcontainer.json",
            c,
            userland,
            caps(),
        )
        .unwrap()
        .kappa()
    };
    assert_eq!(mk(cfg), mk(cfg), "same source ⇒ same κ");
    assert_ne!(mk(cfg), mk(other), "different config ⇒ different identity");
}
