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

A holospace’s environment — an operating system and a repository — is
**canonical content**: κ-labels for what the bytes *are*, never where
they live (Law L1). **Booting** realizes the holospace as a
**computation over that content**: a κ-addressed **execution
codemodule** reads the environment by κ, runs it, and writes new
canonical state. For a general operating system the codemodule is a
**system emulator** compiled to Wasm and bound to the `hologram`
operations (ADR-009): the OS image is its content-addressed store of
blocks, the console / input / network are **canonical events on hologram
channels** (`publish` / `subscribe`), and the running state is a **κ
snapshot** (suspend / resume / migrate). The emulator and the image are
imported and verified trustlessly like any κ (the substrate’s
content-addressed read, `get_with_fetch`). Nothing is named by location
— the image is a κ, a "file" is content, state is a κ — so an
**arbitrary** operating system (Linux first, then any the emulator
boots) runs the uor-native way: content in, computation through the
substrate, canonical state out. The same holospace κ runs on any peer
(Q6) and re-derives to verify (L5). This rests on the κ-addressed Wasm
code-module surface (the Execution Surface building block, ADR-008’s
contract — still in force; the emulator is itself such a code module).
Conformance: `CC-6` through `CC-11` (Chapter 10).

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

## Projection (a surface over a holospace)

The operator’s interfaces are **projections** — surfaces that render and
drive a holospace’s canonical content, never a second source of truth
and never a server (Law L1, L3). The **Platform Manager** is the
management projection: an operator signs in and sees, provisions, and
acts on their holospaces. A **workspace projection** — an editor, a file
tree, and a terminal — gives the
[Codespaces](https://github.com/features/codespaces) /
[Gitpod](https://www.gitpod.io) experience over a **running** holospace:
it reads the environment’s content by κ and publishes the operator’s
input as canonical events on the holospace’s channels, so editing a file
or running a command is content in, computation through the substrate,
canonical state out. A projection holds no state of its own — it is a
view and an intent surface over content (Law L3, the store is the
memory). Delivered from the cold-start gateway (GitHub Pages), a
projection makes the browser a peer that **is** the substrate.

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
