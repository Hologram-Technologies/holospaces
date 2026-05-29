# Architecture Constraints

## The laws (non-negotiable)

These invariants hold everywhere in holospaces. They are constraints,
not aspirations.

| \#  | Law                                  | Constraint it imposes                                                                                                                                                                            |
|-----|--------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| L1  | **Content, not location**            | No servers; nothing is identified by host, path, or URL. Identity is the κ-label.                                                                                                                |
| L2  | **Canonical forms only**             | Operate on canonical forms; hold κ-labels, not objects; canonicalize at the ingest boundary and never leave canonical form.                                                                      |
| L3  | **The store is the memory**          | The [hologram](https://github.com/Hologram-Technologies/hologram) content-addressed store is the address space; RAM is a cache; a "page fault" is a κ-resolve, "eviction" is garbage collection. |
| L4  | **Everything through the substrate** | No parallel memory, storage, network, or runtime; holospaces is a thin layer of operations over the substrate.                                                                                   |
| L5  | **Verify by re-derivation**          | Re-derive every received byte against its κ before accepting it.                                                                                                                                 |

## Technical constraints

| \#  | Constraint                                                                                                                                                                                                                                                                                                                             |
|-----|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| T1  | Built on the [UOR Framework](https://github.com/UOR-Foundation/UOR-Framework) stack: [UOR-ADDR](https://github.com/UOR-Foundation/uor-addr) content addressing, [Prism](https://github.com/UOR-Foundation/prism), and the [hologram](https://github.com/Hologram-Technologies/hologram) substrate (compute + storage/network/runtime). |
| T2  | hologram is a **private** repository, consumed as git dependencies (its member crates); `uor-addr` and `uor-foundation` come from crates.io; Prism arrives transitively. Cargo fetches the private dependency via credentialed git.                                                                                                    |
| T3  | The browser peer is delivered cold-start from **GitHub Pages**, which acts only as an untrusted, content-addressed gateway (verified on receipt, per L5).                                                                                                                                                                              |
| T4  | Toolchain: Rust (+ `wasm-pack`), Lean 4, and the documentation pipeline (see below). Defined in the devcontainer.                                                                                                                                                                                                                      |

## Documentation constraints

The documentation is **authoritative for holospaces** and is governed by
three external standards (see `docs/docs-definition.md`): **arc42 + C4**
(architecture), **OPM ISO 19450** (conceptual model), and **ISO/IEC/IEEE
15288** (lifecycle). It is maintained without gaps, inconsistencies, or
assumptions; every implemented feature and workspace artifact links back
to it; and external systems are referenced by hyperlink, never by
restating their internals.
