# Deployment View

## Infrastructure Level 1

A holospaces *peer* is any environment that **becomes** the substrate by
realizing it locally — it does not connect to a server (Law L1). The
same holospace κ boots on any peer.

| Peer           | Realization (storage · network · execution)                                                            | Notes                                                                                                         |
|----------------|--------------------------------------------------------------------------------------------------------|---------------------------------------------------------------------------------------------------------------|
| **Browser**    | hologram’s OPFS storage · HTTP content-addressed gateway · the executor compiled to Wasm (`wasm-pack`) | Cold-started from GitHub Pages (untrusted gateway, verified on receipt). Hosts the Hologram Platform Manager. |
| **Native**     | hologram’s native storage · content-addressed networking · the native executor                         | A full peer; can both serve and route content.                                                                |
| **Bare-metal** | hologram’s block-device storage · bare networking · the no-std executor                                | Boots from firmware; the leanest peer.                                                                        |

The storage, network, and runtime backends are defined and implemented
in [hologram](https://github.com/Hologram-Technologies/hologram) (the
`substrate/` family); holospaces composes them per environment and
supplies the `.holo` execution backend. Backend contracts are
authoritative there, referenced here.

## Infrastructure Level 2

Each peer maps the same logical roles onto environment-specific
mechanisms:

- **Storage** — the content-addressed store is the address space (Law
  L3); a κ-resolve is local-first, else fetched over the network and
  verified.

- **Network** — content-addressed routing between peers (no host/peer-id
  naming, Law L1); the rendezvous/routing mechanism is hologram’s (see
  [hologram](https://github.com/Hologram-Technologies/hologram)).

- **Execution** — the `.holo` Engine runs compute artifacts;
  devcontainer holospaces run as hologram-native containers.

A single online peer is simply the one-participant case of the
content-addressed mesh; signing in with the same identity links an
operator’s peers so their holospaces synchronise (Chapter 8, Identity
and sync).
