# Cross-cutting Concepts

This chapter is the prose conceptual model; the formal model is given in
Object-Process Methodology in the Conceptual Model chapter (OPM, ISO
19450). Concepts that belong to the
[hologram](https://github.com/Hologram-Technologies/hologram) substrate
are described here only as holospaces *uses* them, and linked for their
authoritative definitions.

## Canonical forms and κ-labels

Everything in holospaces is a *canonical form*: deterministic bytes
identified by a **κ-label** (a content address, `<axis>:<hex>` =
`H(canonical_form)`), supplied by
[UOR-ADDR](https://github.com/UOR-Foundation/uor-addr). Identity is
*what a thing is*, never *where it is* (Law L1). Data, code, images,
running state, and holospaces' own state are all κ.

## The substrate (the one medium)

holospaces holds and moves all content through hologram’s three
content-addressed pillars — storage (KappaStore), network (KappaSync),
and runtime (ContainerRuntime) — and adds none of its own (Law L4).
Their contracts are authoritative in
[hologram](https://github.com/Hologram-Technologies/hologram).

## Two compute forms

Executable content takes two κ-addressed shapes: a **Wasm code module**
(general/system code, run by hologram’s runtime via its
`ContainerEngine` seam) and a **tensor `.holo`** (compute, run by
hologram’s executor) — both contracts defined by
[hologram](https://github.com/Hologram-Technologies/hologram). A
holospace is built from one or the other; the Manager manages both
uniformly.

## The execution surface

A devcontainer holospace’s Linux/POSIX surface maps onto the Wasm
code-module form: its userland is a **Wasm-recompiled userland** — a
κ-addressed Wasm module that imports only the substrate’s host ABI (the
`hologram` host module, the syscall boundary) and presents the container
ABI hologram’s runtime drives (ADR-008, resolving RT1). A
`devcontainer.json` *selects* a κ-addressed userland (content) rather
than naming an OCI image (location, forbidden by Law L1). This keeps
code identity content, dedup and verification uniform, and execution on
the one substrate medium (Laws L1/L2/L4). holospaces ingests, validates,
binds, and **boots** this surface on every peer — through hologram’s
`ContainerRuntime` over a per-environment `ContainerEngine` (Wasmtime
natively; the `wasmi` interpreter in the browser and on bare-metal). A
userland is κ-addressed content the platform hosts. Conformance: `CC-6`.

## The holospace

A **holospace** is a bootable, κ-addressed environment — the unit
holospaces provisions, runs, and manages. Its identity is the κ of its
definition, so the same definition always yields the same holospace
(reproducibility, Q4). It is provisioned from a holo-file or a
devcontainer; booting resolves its κ and spawns it through the runtime
with its capabilities.

## Capabilities

A holospace is spawned with a **capability set** that bounds what it may
touch — its storage roots, the channels it may use, and its resource
budgets. Capabilities are content: a capability set is itself
κ-addressed, so the authority a holospace runs under is part of its
reproducible definition and is verifiable. The capability model (how
authority is represented, delegated, and enforced) is hologram’s;
holospaces composes it (see
[hologram](https://github.com/Hologram-Technologies/hologram)).

## The store is the memory

The content-addressed store is the address space; RAM is a cache (Law
L3). Resolving a κ is the "page fault" (local, else fetched and
verified); garbage collection is "eviction"; identical content is stored
once (dedup). This is what lets a holospace exceed available RAM.

## Identity and sync

The operator signs in by unlocking a self-sovereign key — not a server
account. That identity links the operator’s instances so their
holospaces and state synchronise over the substrate; because state is
content (a snapshot is a κ), a holospace can be suspended on one
instance and resumed on another.

## Verify by re-derivation

Trust is in the math, not the source: every received byte is accepted
only after re-deriving its κ (Law L5). This is what makes an untrusted
gateway (e.g. GitHub Pages, or any peer) safe to fetch from.

## *\<Concept 1\>*

*arc42 template structural anchor; the concrete cross-cutting concepts
are the named sections above.*

## *\<Concept 2\>*

*arc42 template structural anchor; the concrete cross-cutting concepts
are the named sections above.*

## *\<Concept n\>*

*arc42 template structural anchor; the concrete cross-cutting concepts
are the named sections above.*
