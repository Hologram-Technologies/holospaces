# Deployment View

## Infrastructure Level 1

A holospaces *peer* is any environment that **becomes** the substrate by
realizing it locally — it does not connect to a server (Law L1). The
same holospace κ boots on any peer.

Each peer composes, for its environment, hologram’s storage and
content-addressed networking backends, a `ContainerEngine` that boots
Wasm userlands (the execution surface, ADR-008), and hologram’s `.holo`
executor for tensor compute. The two execution forms (Chapter 8) run on
these two engines; the `ContainerEngine` is environment-specific:

| Peer           | Container engine (Wasm userlands)                                                                                                   | `.holo` executor                       | Notes                                                                                                                                                              |
|----------------|-------------------------------------------------------------------------------------------------------------------------------------|----------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Browser**    | hologram’s `wasmi` interpreter engine, compiled to Wasm (`wasm-pack`) — a JIT cannot run in the browser sandbox, an interpreter can | the hologram executor compiled to Wasm | Cold-started from GitHub Pages (untrusted gateway, verified on receipt). Hosts the Hologram Platform Manager, which boots userland containers in-browser (`CC-6`). |
| **Native**     | hologram’s Wasmtime engine (JIT)                                                                                                    | the hologram executor, native          | A full peer; can both serve and route content.                                                                                                                     |
| **Bare-metal** | hologram’s `wasmi` interpreter engine (`no_std`, no OS)                                                                             | the hologram executor                  | Boots from firmware; the leanest peer. The same interpreter engine as the browser.                                                                                 |

All backends — storage, network, both container engines
(`hologram-runtime-wasmtime`, `hologram-runtime-bare`), and the `.holo`
executor — are defined and implemented in
[hologram](https://github.com/Hologram-Technologies/hologram) (the
`substrate/` family); holospaces composes them per environment, never
re-implementing them (ADR-003, ADR-006). The same holospace κ boots on
any of these engines (Q6) — witnessed for the native (Wasmtime) and
browser/bare-metal (`wasmi`) engines by `CC-6`.

## Infrastructure Level 2

Each peer maps the same logical roles onto environment-specific
mechanisms:

- **Storage** — the content-addressed store is the address space (Law
  L3); a κ-resolve is local-first, else fetched over the network and
  verified.

- **Network** — content-addressed routing between peers (no host/peer-id
  naming, Law L1); the rendezvous/routing mechanism is hologram’s (see
  [hologram](https://github.com/Hologram-Technologies/hologram)).

- **Execution** — two engines: a `ContainerEngine` boots Wasm userlands
  (a devcontainer holospace’s Linux/POSIX surface, ADR-008) through
  hologram’s `ContainerRuntime`; the `.holo` executor runs tensor
  compute artifacts. The `ContainerEngine` is Wasmtime natively and the
  `wasmi` interpreter in the browser and on bare-metal.

A single online peer is simply the one-participant case of the
content-addressed mesh; signing in with the same identity links an
operator’s peers so their holospaces synchronise (Chapter 8, Identity
and sync).
