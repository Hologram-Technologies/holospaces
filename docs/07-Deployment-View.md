# Deployment View

## Infrastructure Level 1

A holospaces *peer* is any environment that **becomes** the substrate by
realizing it locally — it does not connect to a server (Law L1). The
same holospace κ boots on any peer.

Each peer composes, for its environment, hologram’s storage and
content-addressed networking backends, a `ContainerEngine` that boots
Wasm code modules — including the **system-emulator codemodule** that
computes an arbitrary OS (the execution surface, ADR-009) — and
hologram’s `.holo` executor for tensor compute. The two execution forms
(Chapter 8) run on these two engines; the `ContainerEngine` is
environment-specific:

| Peer           | Container engine (Wasm code modules)                                                                                                | `.holo` executor                       | Notes                                                                                                                                                                                                  |
|----------------|-------------------------------------------------------------------------------------------------------------------------------------|----------------------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Browser**    | hologram’s `wasmi` interpreter engine, compiled to Wasm (`wasm-pack`) — a JIT cannot run in the browser sandbox, an interpreter can | the hologram executor compiled to Wasm | Cold-started from GitHub Pages (untrusted gateway, verified on receipt). Delivers the Platform Manager and workspace projections; boots holospaces in-browser, including the system emulator (`CC-6`). |
| **Native**     | hologram’s Wasmtime engine (JIT)                                                                                                    | the hologram executor, native          | A full peer; can both serve and route content.                                                                                                                                                         |
| **Bare-metal** | hologram’s `wasmi` interpreter engine (`no_std`, no OS)                                                                             | the hologram executor                  | Boots from firmware; the leanest peer. The same interpreter engine as the browser.                                                                                                                     |

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
  [hologram](https://github.com/Hologram-Technologies/hologram)). A
  *sandboxed* peer (a browser tab) has no NIC, so a guest’s arbitrary
  internet (`curl`/`apt`, a `git` clone, an outbound socket) exits through
  one of three surfaces, all speaking the same egress protocol (CC-16) and
  all **content-blind** (SEC-7): a **holospaces-node** you flash (CC-39); the
  **mesh** to an exit peer (CC-38); or — making a Chromebook fully
  self-contained — a **local Chrome extension** that opens the raw sockets a
  tab cannot, via the Direct Sockets API
  (`crates/holospaces-web/extension/`). A *sovereign* peer (a bare-metal
  holospace, Chromium-on-device) has a real NIC and needs none of them.

- **Execution** — two engines: a `ContainerEngine` boots Wasm code
  modules — including the system-emulator codemodule that computes an
  arbitrary OS (ADR-009) — through hologram’s `ContainerRuntime`; the
  `.holo` executor runs tensor compute artifacts. The `ContainerEngine`
  is Wasmtime natively and the `wasmi` interpreter in the browser and on
  bare-metal.

- **Projection** — the operator’s surfaces (the Platform Manager GUI; a
  workspace editor/terminal over a running holospace, Chapter 8) are
  delivered from the cold-start gateway (GitHub Pages) and run **on the
  peer**; they render and drive content, holding no state of their own
  (Law L3). Launching a holospace opens its workspace projection.

A single online peer is simply the one-participant case of the
content-addressed mesh; signing in with the same identity links an
operator’s peers so their holospaces synchronise (Chapter 8, Identity
and sync).
