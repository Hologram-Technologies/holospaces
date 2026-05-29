# AGENTS.md — holospaces

This file is **timeless** orientation: durable facts about UOR, Hologram, and holospaces concepts, and how we work. **It contains no status** — nothing that goes stale (no progress, no crate inventories, no "current state"). The **authoritative specification** is the documentation under [`docs/`](docs/) — architecture, conceptual model, and lifecycle (start at [docs/docs-definition.md](docs/docs-definition.md)). Every implemented feature and workspace artifact must link back to it; this file is orientation, not a second source of truth.

## What holospaces is

holospaces is a UOR-native **boot layer** over the hologram substrate. It provisions and runs **holospaces** — bootable, content-addressed environments — and ships **Hologram**, the first-party holospace whose console (the **Platform Manager**) manages the rest. For the dev-environment use case it is a UOR-native, serverless Gitpod/Codespaces.

## The stack it builds on

- **UOR Framework** (`UOR-Foundation/UOR-Framework`) — the formal model: content-addressing grounded in the ring Z/(2ⁿ)Z.
- **uor-foundation** / **Prism** (`uor-prism`) — the substrate crate (Layer 1) and standard library (Layer 2).
- **UOR-ADDR** (`uor-addr`) — content addressing: a **κ-label** is `<axis>:<hex>` = `H(canonical_form)`.
- **hologram** (`Hologram-Technologies/hologram`) — the substrate: a `.holo` tensor runtime (`crates/`) and the deployment substrate (`substrate/`: `KappaStore` · `KappaSync` · `ContainerRuntime`).

## Concepts every contributor must know

- **κ-label / canonical form** — identity is content (*what*, not *where*).
- **The substrate's three pillars** — storage (`KappaStore`), network (`KappaSync`), runtime (`ContainerRuntime`).
- **Two compute forms** — Wasm code modules (general/system code, via the `ContainerEngine` under the closed `hologram.*` ABI) and tensor `.holo` (compute, via `hologram-exec`).
- **holospace** — the bootable, κ-addressed unit; provisioned from a holo-file or a devcontainer; managed by the Manager.
- **peer** — an environment (browser / native / bare-metal) that *becomes* the substrate by running a holospace.
- **operator / sign-in** — a self-sovereign identity that syncs an operator's holospaces across instances (not a server account).

## The laws (non-negotiable)

1. **Content, not location** — no servers; no host/path/URL as identity.
2. **Canonical forms only** — operate on canonical forms; hold κ, not objects; canonicalize at the ingest boundary.
3. **The store is the memory** — `KappaStore` is the address space; RAM is a cache.
4. **Everything through the substrate** — no parallel memory/storage/network/runtime.
5. **Verify by re-derivation** — re-derive every received byte against its κ.

## How we work

- **The docs are authoritative.** The specification under `docs/` (arc42 chapters, ADRs in chapter 09, the OPM conceptual model, the ISO 15288 lifecycle) is the **single source of truth**, maintained **without gaps, inconsistencies, or assumptions**. It precedes code, and **every implemented feature and part of the workspace links back to it**.
- **V&V by external ground truth — never self-reference.** Validate against **imported external artifacts**: e.g. native `hologram-exec` outputs as the oracle for the browser engine; the substrate TCK; σ-axis KAT vectors. Imported artifacts are themselves κ-addressed and verified on import.
- **Stay timeless.** This file and the `docs/` specification hold only durable content — implementation status is reflected by V&V/CI and git, never by a narrative status doc.

## Consuming the stack

hologram is a **private** repository, consumed as **git dependencies** (its member crates, tracking `main`); `uor-addr` and `uor-foundation` come from **crates.io**, and Prism arrives transitively. Cargo fetches the private git dependency via credentialed git (`[net] git-fetch-with-cli = true`).
