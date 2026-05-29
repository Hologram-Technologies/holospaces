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
(general/system code, run by the substrate’s ContainerEngine) and a
**tensor `.holo`** (compute, run by the hologram executor). A holospace
is built from one or the other; the Manager manages both uniformly.

## The holospace

A **holospace** is a bootable, κ-addressed environment — the unit
holospaces provisions, runs, and manages. Its identity is the κ of its
definition, so the same definition always yields the same holospace
(reproducibility, Q4). It is provisioned from a holo-file or a
devcontainer; booting resolves its κ and spawns it through the runtime
with its capabilities.

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
