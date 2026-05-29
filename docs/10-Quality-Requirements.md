# Quality Requirements

## Quality Requirements Overview

The quality goals (Chapter 1) derive from the laws (Chapter 2):

| Attribute                | Requirement                                                                                     |
|--------------------------|-------------------------------------------------------------------------------------------------|
| **Integrity**            | Every accepted byte is re-derived against its κ (Law L5).                                       |
| **Reproducibility**      | A holospace’s identity is the κ of its definition; identical inputs yield identical holospaces. |
| **Portability**          | The same holospace κ boots on any peer (browser / native / bare-metal).                         |
| **Efficiency (memory)**  | The store is the address space; content dedupes; RAM is a bounded cache (Law L3).               |
| **Autonomy (no server)** | No host, no account, no control plane; peers are content-addressed (Law L1).                    |
| **Authority**            | The documentation is the single authoritative source; features trace to it.                     |

## Quality Scenarios

| \#  | Scenario                                                                                  | Expected response                                                                      |
|-----|-------------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------|
| QS1 | The same git repo + `devcontainer.json` is provisioned twice, on different peers.         | Both yield the same holospace κ.                                                       |
| QS2 | A holospace is suspended on one signed-in instance and resumed on another.                | It resumes from the κ snapshot; unchanged state dedupes.                               |
| QS3 | A gateway (e.g. GitHub Pages, or a peer) returns bytes that do not match the requested κ. | The bytes are rejected on re-derivation; the fetch fails over to another source.       |
| QS4 | A holospace’s total state exceeds available RAM.                                          | It still boots; content is demand-paged by κ-resolve, evicted by garbage collection.   |
| QS5 | An operator signs in on a new instance.                                                   | Their holospaces and state are discoverable and synchronise over the substrate.        |
| QS6 | A contributor implements a feature.                                                       | The feature links to the documentation section it realizes; the V1–V8 pipeline passes. |

## Verification and Validation

holospaces is evaluated against **external authoritative specifications
and standards** — never against itself (no self-reference). The V&V is
**defined by this documentation** and is authoritative: each invariant
below names the external authority it is validated against, the
mechanism that enforces it, and the witness. Real validation artifacts
are imported and pinned, with provenance recorded in `vv/PROVENANCE.md`;
imported artifacts are themselves content-addressed and verified on
import. The executable framework lives in `vv/` and is the single V&V
entry point (`vv/run.sh`).

The framework has two tiers:

- **Specification conformance** (`CS`) — the holospaces specification
  (this documentation) validated against the standards it is authored
  under. Live on every build.

- **Component conformance** (`CC`) — each implemented component
  validated against the external authority for its behavior. Each is
  specified here and witnessed as that component is built
  (conformance-driven), never by self-reference.

## Conformance catalog

| ID   | Invariant                                         | External authority                                                                                                                 | Enforcement                                                              | Witness                                 |
|------|---------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------|--------------------------------------------------------------------------|-----------------------------------------|
| CS-1 | Architecture structure conforms to arc42          | [arc42](https://arc42.org) template (pinned)                                                                                       | V1, V2                                                                   | every doc build (live)                  |
| CS-2 | The C4 model is well-formed                       | Structurizr DSL                                                                                                                    | V3                                                                       | every doc build (live)                  |
| CS-3 | Rendered docs are valid GitHub-flavoured Markdown | CommonMark / GFM; GitHub-markup                                                                                                    | V4, V5                                                                   | every doc build (live)                  |
| CS-4 | The conceptual model is valid OPM                 | OPM (ISO 19450) OPL grammar                                                                                                        | V6                                                                       | every doc build (live)                  |
| CS-5 | Each OPD agrees with its OPL                      | OPM (ISO 19450) bimodality                                                                                                         | V7                                                                       | every doc build (live)                  |
| CS-6 | The lifecycle covers the standard processes       | ISO/IEC/IEEE 15288                                                                                                                 | V8                                                                       | every doc build (live)                  |
| CC-1 | κ-labels are correct content addresses            | the σ-axis hash standards (BLAKE3; FIPS 180-4 SHA-2; FIPS 202 SHA-3; Keccak) via their reference implementations                   | byte-for-byte equality with the reference σ-axis; re-derivation (Law L5) | live (`vv/suites/cc1-kappa-addressing`) |
| CC-2 | The browser `.holo` engine equals the native one  | the native [hologram](https://github.com/Hologram-Technologies/hologram) executor as oracle (identical `.holo` yields identical κ) | differential check                                                       | live (`vv/suites/cc2-holo-engine`)      |
| CC-3 | A peer’s storage obeys the substrate contract     | the [hologram](https://github.com/Hologram-Technologies/hologram) substrate conformance battery (TCK)                              | run the imported battery                                                 | live (`vv/suites/cc3-substrate-tck`)    |
| CC-4 | A devcontainer holospace matches its source       | the [Dev Container](https://containers.dev) and OCI image specifications                                                           | reproducible-κ check (Q4)                                                | live (`vv/suites/cc4-devcontainer`)     |
| CC-5 | Wasm code modules are specification-valid         | the [WebAssembly](https://webassembly.org) specification                                                                           | module validation                                                        | live (`vv/suites/cc5-wasm`)             |

The `CS` rows are witnessed green by every documentation build. The `CC`
rows are each witnessed by a suite in `vv/suites/` that validates the
component against its external authority — never against itself — by
composing the
[hologram](https://github.com/Hologram-Technologies/hologram) substrate
(consumed by reference, ADR-006), at the rev pinned in the workspace
`Cargo.toml`. Adding a component without satisfying its `CC` row leaves
holospaces incomplete by definition.

## Quality gates and test tiers

The quality commitments above are enforced from the first commit by
continuous integration (`.github/workflows/ci.yml`), which runs on every
change:

- **V&V** — `vv/run.sh`: CS-1..CS-6 specification conformance (V1–V8)
  and the CC-1..CC-5 component suites (`vv/suites/`), each against its
  external authority.

- **Format** — `cargo fmt --check`.

- **Lints** — `cargo clippy` with warnings denied; the workspace forbids
  `unsafe_code` and warns on `missing_docs`.

- **Docs** — `cargo doc` with warnings denied (no broken links; every
  public item documented).

- **Unit** — `cargo test --lib`: a building block in isolation.

- **Integration** — `cargo test --test integration`: building blocks
  composed.

- **End-to-end** — `cargo test --test e2e`: whole operator flows (over
  the real substrate runtime and a real content-addressed gateway).

- **Portability** — the peer core builds for every supported
  environment: native, browser (`--target wasm32-unknown-unknown`), and
  bare-metal (`--no-default-features --target thumbv7em-none-eabi`,
  `no_std`). The same holospace κ boots on any peer (Chapter 7; quality
  goal Q6).

- **Browser** — `scripts/browser-manager-test.sh`: the Hologram Platform
  Manager (the browser peer) runs in headless Chromium (Playwright) —
  sign in, provision, view, resolve + verify by re-derivation (Law L5).
  It is deployed to GitHub Pages by `.github/workflows/pages.yml` (the
  cold-start, untrusted gateway of Chapter 6).

The `CC` suites and the test tiers produce each component’s `CC` witness
against its external authority. The gates exist and run from the first
commit; the test tiers and `CC` suites are populated as components land.
