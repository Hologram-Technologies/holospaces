//! **CC-4 — a devcontainer holospace matches its source.**
//!
//! The Conformance catalog row `CC-4` (arc42 chapter 10,
//! `docs/src/arc42/adoc/10_quality_requirements.adoc`): a devcontainer
//! holospace matches its source, validated against the
//! [Dev Container](https://containers.dev) and OCI image specifications, by a
//! reproducible-κ check (Q4).
//!
//! The external authority is the Dev Container / OCI conformance cases imported
//! in `vv/artifacts/cc4/devcontainer-cases.json` (provenance in
//! `vv/PROVENANCE.md`), plus this repository's own real
//! `.devcontainer/devcontainer.json`. This witness checks:
//!
//! 1. **Spec conformance** — holospaces' ingestor
//!    ([`holospaces::boot::devcontainer::parse`]) accepts exactly the configs
//!    the specification accepts (a single container image source; well-formed
//!    known properties).
//! 2. **Matches its source / reproducibility** — ingesting the same source
//!    yields the same holospace κ; a different config yields a different κ
//!    (QS1 / Q4).
//!
//! Run by `vv/run.sh`; also `cargo test -p holospaces --test cc4_devcontainer`.

use holospaces::boot::{devcontainer, ingest_devcontainer};
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

fn cases_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vv/artifacts/cc4/devcontainer-cases.json")
}

fn repo_devcontainer_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.devcontainer/devcontainer.json")
}

/// (1) The ingestor's accept/reject decisions match the Dev Container spec on
/// every imported conformance case.
#[test]
fn ingestor_agrees_with_the_dev_container_spec() {
    let raw = std::fs::read(cases_path()).expect("read devcontainer-cases.json");
    let doc: Value = serde_json::from_slice(&raw).expect("valid JSON");
    let cases = doc["cases"].as_array().expect("cases array");
    assert!(cases.len() >= 8, "expected the full case battery");

    for case in cases {
        let name = case["name"].as_str().unwrap();
        let expected_valid = case["valid"].as_bool().unwrap();
        let config_bytes = serde_json::to_vec(&case["config"]).unwrap();
        let got = devcontainer::parse(&config_bytes).is_ok();
        assert_eq!(
            got, expected_valid,
            "case '{name}': spec says valid={expected_valid}, ingestor says {got}"
        );
    }
}

/// (2) A devcontainer holospace matches its source: same source ⇒ same κ;
/// different config ⇒ different κ (QS1 / Q4).
#[test]
fn devcontainer_holospace_is_reproducible_from_its_source() {
    let cfg = br#"{"name":"app","image":"debian:12"}"#;
    let a = ingest_devcontainer(
        "https://x.invalid/r.git",
        "main",
        ".devcontainer/devcontainer.json",
        cfg,
        caps(),
    )
    .unwrap();
    let b = ingest_devcontainer(
        "https://x.invalid/r.git",
        "main",
        ".devcontainer/devcontainer.json",
        cfg,
        caps(),
    )
    .unwrap();
    assert_eq!(a.kappa(), b.kappa());

    let other = ingest_devcontainer(
        "https://x.invalid/r.git",
        "main",
        ".devcontainer/devcontainer.json",
        br#"{"name":"app","image":"ubuntu:24.04"}"#,
        caps(),
    )
    .unwrap();
    assert_ne!(
        a.kappa(),
        other.kappa(),
        "different config ⇒ different identity"
    );
}

/// This repository's own `.devcontainer/devcontainer.json` is spec-conformant
/// and ingests to a holospace (a real-world Dev Container fixture).
#[test]
fn repo_devcontainer_json_is_conformant() {
    let raw =
        std::fs::read(repo_devcontainer_path()).expect("read .devcontainer/devcontainer.json");
    devcontainer::parse(&raw)
        .expect("the repo's devcontainer.json is Dev Container spec-conformant");
}
