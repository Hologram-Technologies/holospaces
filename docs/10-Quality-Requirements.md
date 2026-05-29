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
