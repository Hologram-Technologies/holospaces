# Building Block View

## Whitebox Overall System

holospaces, opened, is a thin set of building blocks over the
[hologram](https://github.com/Hologram-Technologies/hologram) substrate.
None of them re-implement substrate functionality; they compose it.

<figure>
<img src="images/c4-l2-holospaces-containers.svg"
alt="Level 2: Containers" />
</figure>

| Building block       | Responsibility                                                                                                                                                                                                                             |
|----------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Realizations**     | holospaces' canonical-form types — chiefly the **holospace** (the κ-addressed bootable unit). Each is IRI-tagged canonical bytes, κ-addressed and verified by re-derivation, using [UOR-ADDR](https://github.com/UOR-Foundation/uor-addr). |
| **Boot Layer**       | The environment-agnostic core: resolve a holospace κ, fetch and verify its parts, and spawn it through hologram’s ContainerRuntime with its capabilities; drive the lifecycle.                                                             |
| **.holo Engine**     | A ContainerEngine backend that runs `.holo` compute artifacts via the [hologram](https://github.com/Hologram-Technologies/hologram) executor. Native, and compiled to Wasm for the browser peer.                                           |
| **Identity**         | Self-sovereign sign-in key and the multi-instance sync keying (an operator’s instances discover and synchronise their holospaces over the substrate).                                                                                      |
| **Platform Manager** | The Hologram platform: the operator console that provisions and manages holospaces. Itself a holospace.                                                                                                                                    |

## Level 2

The Boot Layer is the hub: it depends on Realizations (to resolve a
holospace), on the .holo Engine (to run a holospace whose code is a
`.holo`), and on the hologram substrate (storage / network / runtime).
The Platform Manager drives the Boot Layer; Identity scopes what the
operator’s instances share and sync. The substrate’s contracts —
KappaStore, KappaSync, ContainerRuntime, and the `.holo` executor — are
defined in [hologram](https://github.com/Hologram-Technologies/hologram)
and consumed here by reference.

## Level 3

The Level-3 (component) responsibilities of the building blocks follow
from the design:

**Boot Layer** — an *Ingestor* (canonicalizes a provisioning source, a
git repo + devcontainer or a holo-file, at the boundary into κ-addressed
content, Law L2); a *Resolver* (resolves a holospace κ, fetching and
verifying its parts, Law L5); a *Spawner* (instantiates the holospace
through the substrate runtime with its capabilities); and a *Lifecycle*
component (suspend → κ snapshot, resume, migrate, terminate).

**.holo Engine** — a *ContainerEngine adapter* (the seam the substrate
runtime drives) bound to the
[hologram](https://github.com/Hologram-Technologies/hologram) executor;
in the browser peer this binding is the executor compiled to Wasm.

**Identity** — a *Key store* (the self-sovereign sign-in key) and a
*Sync binding* (scopes which content an operator’s instances announce
and resolve over the substrate).

**Platform Manager** — a *View* (a projection of the operator’s
holospaces and the substrate) and an *Intent* surface (lifecycle and
provisioning actions); its own state is canonical and held in the store
(Law L2).

Each component realizes the Architecture Decisions of Chapter 9 and
applies the Concepts of Chapter 8.
