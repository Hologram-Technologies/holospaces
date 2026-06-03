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

/// This repository's own `.devcontainer/devcontainer.json` is accepted by the
/// ingestor and its declared image source is preserved. If the repo config ever
/// returns to a features-only shape, the ingestor will surface the documented
/// default-image source instead (the Codespaces behavior).
#[test]
fn repo_config_ingests_with_its_declared_image_source() {
    let raw = repo_config();
    let dc = devcontainer::parse(&raw).expect("ingestor accepts the repo config");
    let json = to_canonical_json(&raw).expect("repo config JSONC canonicalizes");
    let value: Value = serde_json::from_slice(&json).expect("repo config is JSON");
    if let Some(image) = value.get("image").and_then(Value::as_str) {
        assert_eq!(
            dc.image_source,
            devcontainer::ImageSource::Image(image.to_owned())
        );
    } else if let Some(build) = value.get("build") {
        let expected = devcontainer::BuildConfig {
            dockerfile: build
                .get("dockerfile")
                .and_then(Value::as_str)
                .unwrap_or("Dockerfile")
                .to_owned(),
            context: build
                .get("context")
                .and_then(Value::as_str)
                .unwrap_or(".")
                .to_owned(),
            args: Default::default(),
        };
        assert_eq!(dc.image_source, devcontainer::ImageSource::Build(expected));
    } else {
        assert_eq!(dc.image_source, devcontainer::ImageSource::Default);
    }
}

/// A *features-only* configuration declares no image source and relies on the
/// implementor's default base image (the Codespaces behavior). The Dev Container
/// *base* schema requires an explicit image source, so it does not model this
/// case; holospaces' ingestor accepts it as the documented default-image superset.
///
/// This asserts the property against a *fixed* features-only config, not the
/// repo's own `.devcontainer/devcontainer.json`: a CI runner or Codespaces
/// prebuild resolves the platform's default base image *into* the live repo file
/// (e.g. `mcr.microsoft.com/devcontainers/base:ubuntu-24.04`), which is exactly
/// the resolution this test must not depend on.
#[test]
fn a_features_only_config_ingests_as_default_image() {
    let cfg = br#"{
        "name": "features-only",
        "features": { "ghcr.io/devcontainers/features/rust:1": {} },
        "customizations": { "vscode": { "extensions": ["rust-lang.rust-analyzer"] } }
    }"#;
    let dc = devcontainer::parse(cfg).expect("ingestor accepts a features-only config");
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

/// (3) The devcontainer spec manages the workbench's base extensions: holospaces
/// parses `customizations.vscode.extensions` (the list a Codespace would
/// auto-install) and that list drives what holospaces installs from the open
/// gallery — it bundles none by default. The config is validated against the
/// published schema first (the spec is the judge), then holospaces extracts the
/// declared extension ids.
#[test]
fn devcontainer_extensions_are_parsed_from_the_spec_customizations() {
    let schema = schema();
    let config = serde_json::json!({
        "name": "rust-dev",
        "image": "mcr.microsoft.com/devcontainers/rust:1",
        "forwardPorts": [3000, "db:5432"],
        "postCreateCommand": "cargo fetch",
        "postStartCommand": ["echo", "started"],
        "remoteEnv": { "RUST_LOG": "debug", "EMPTY": null },
        "customizations": {
            "vscode": {
                "extensions": ["rust-lang.rust-analyzer", "tamasfe.even-better-toml"],
                "settings": { "editor.formatOnSave": true }
            }
        }
    });
    // The schema (the external authority) accepts this real-shaped config.
    assert!(
        schema.is_valid(&config),
        "the full managed-parameters config is valid per the Dev Container schema"
    );
    let bytes = serde_json::to_vec(&config).unwrap();

    // holospaces parses the declared base extensions (the spec's extension
    // management) — exactly the list a Codespace auto-installs.
    let dc = devcontainer::parse(&bytes).expect("parse the devcontainer");
    assert_eq!(
        dc.extensions,
        vec![
            "rust-lang.rust-analyzer".to_string(),
            "tamasfe.even-better-toml".to_string()
        ],
        "the base extensions are taken from customizations.vscode.extensions"
    );

    // The other managed parameters the spec defines (CC-21/22/23 consume these):
    assert_eq!(
        dc.forward_ports,
        vec![3000u16, 5432],
        "forwardPorts (integer / \"host:port\") → the forwarded ports (CC-21)"
    );
    assert_eq!(
        dc.lifecycle,
        vec![
            (
                devcontainer::LifecycleHook::PostCreate,
                "cargo fetch".to_string()
            ),
            (
                devcontainer::LifecycleHook::PostStart,
                "echo started".to_string()
            ),
        ],
        "the lifecycle commands, in spec run-order, normalized to shell lines (CC-22)"
    );
    assert_eq!(
        dc.remote_env.get("RUST_LOG").map(String::as_str),
        Some("debug"),
        "remoteEnv is applied into the devcontainer (CC-23); a null value is skipped"
    );
    assert!(
        !dc.remote_env.contains_key("EMPTY"),
        "a null remoteEnv value is dropped"
    );

    // A config with no customizations declares no base extensions (holospaces
    // bundles none — no lock-in).
    let bare = serde_json::to_vec(&serde_json::json!({ "image": "debian:12" })).unwrap();
    assert!(
        devcontainer::parse(&bare).unwrap().extensions.is_empty(),
        "no customizations ⇒ no bundled extensions"
    );

    // A malformed extensions list is rejected (a non-string id).
    let bad = serde_json::to_vec(
        &serde_json::json!({ "image": "debian:12", "customizations": { "vscode": { "extensions": [42] } } }),
    )
    .unwrap();
    assert!(
        devcontainer::parse(&bad).is_err(),
        "a malformed extension id is refused"
    );
}
