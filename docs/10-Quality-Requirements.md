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

| ID   | Invariant                                         | External authority                                                                                                                 | Enforcement                                  | Witness                                          |
|------|---------------------------------------------------|------------------------------------------------------------------------------------------------------------------------------------|----------------------------------------------|--------------------------------------------------|
| CS-1 | Architecture structure conforms to arc42          | [arc42](https://arc42.org) template (pinned)                                                                                       | V1, V2                                       | every doc build (live)                           |
| CS-2 | The C4 model is well-formed                       | Structurizr DSL                                                                                                                    | V3                                           | every doc build (live)                           |
| CS-3 | Rendered docs are valid GitHub-flavoured Markdown | CommonMark / GFM; GitHub-markup                                                                                                    | V4, V5                                       | every doc build (live)                           |
| CS-4 | The conceptual model is valid OPM                 | OPM (ISO 19450) OPL grammar                                                                                                        | V6                                           | every doc build (live)                           |
| CS-5 | Each OPD agrees with its OPL                      | OPM (ISO 19450) bimodality                                                                                                         | V7                                           | every doc build (live)                           |
| CS-6 | The lifecycle covers the standard processes       | ISO/IEC/IEEE 15288                                                                                                                 | V8                                           | every doc build (live)                           |
| CC-1 | κ-labels are correct content addresses            | the σ-axis hash standards (BLAKE3; FIPS 180-4 SHA-2; FIPS 202 SHA-3; Keccak) and their published test vectors                      | re-derivation against imported KATs (Law L5) | with the κ-addressing / `Realizations` component |
| CC-2 | The browser `.holo` engine equals the native one  | the native [hologram](https://github.com/Hologram-Technologies/hologram) executor as oracle (identical `.holo` yields identical κ) | differential check                           | with the `.holo` Engine                          |
| CC-3 | A peer’s storage obeys the substrate contract     | the [hologram](https://github.com/Hologram-Technologies/hologram) substrate conformance battery (TCK)                              | run the imported battery                     | with a peer store                                |
| CC-4 | A devcontainer holospace matches its source       | the [Dev Container](https://containers.dev) and OCI image specifications                                                           | reproducible-κ check (Q4)                    | with the devcontainer ingestor                   |
| CC-5 | Wasm code modules are specification-valid         | the [WebAssembly](https://webassembly.org) specification                                                                           | module validation                            | with the Wasm execution path                     |

The `CS` rows are witnessed green by every documentation build (the
specification is the part of holospaces implemented today). The `CC`
rows are the authoritative requirement each component must meet against
its external authority; they are witnessed as the component is
implemented. Adding a component without satisfying its `CC` row leaves
holospaces incomplete by definition.
