# Runtime View

The runtime scenarios below show how holospaces behaves over time. (The
arc42 template placeholder scenario headings are retained at the end for
structural conformance.)

## Provisioning a holospace from a devcontainer

1.  The operator gives the Manager a repository with a valid
    `devcontainer.json`.

2.  holospaces ingests the repository, the config, and the
    operating-system image as **κ-addressed content** — fetched and
    verified through the substrate (`get_with_fetch`), addressed by what
    they *are*, never a located image (Laws L1/L2/L5); shared content
    dedupes.

3.  It composes a **holospace definition** — a κ over those parts (the
    OS image, the repository, the system-emulator codemodule, the
    capabilities); the same source yields the same κ (reproducibility,
    Q4).

4.  The Boot Layer boots the holospace: the **system-emulator
    codemodule** (ADR-009) computes the operating system from its
    κ-addressed content over the substrate runtime — disk as κ-addressed
    blocks, console / input / network as hologram channels, running
    state as a κ snapshot.

5.  The holospace appears in the Manager, ready to open.

## Provisioning a holospace from a holo-file

1.  The operator supplies a `.holo` artifact (e.g. a model compiled by
    [hologram-ai](https://github.com/Hologram-Technologies/hologram-ai))
    by its κ — reproducible by definition, since the κ *is* the
    artifact’s content address (Q4).

2.  The Boot Layer resolves the κ (locally, else fetched and verified,
    Law L5) and runs it via the **.holo Engine** over the
    [hologram](https://github.com/Hologram-Technologies/hologram)
    executor (a tensor-compute run, distinct from the Wasm
    `ContainerEngine`).

3.  Management is identical to any other holospace — the same
    κ-identity, provisioning, resolution, and migration; the two compute
    forms differ only in which substrate engine executes them (the
    executor for a `.holo`, the `ContainerRuntime` for a Wasm code
    module such as the system emulator).

## Suspend, resume, and migrate

1.  The operator suspends a running holospace; its state is captured as
    a **κ snapshot** (state is content).

2.  Because the snapshot is a κ, any of the operator’s instances that
    can resolve it may **resume** it — including a different
    environment.

3.  This makes migration a content operation: suspend on one peer,
    resume on another; unchanged state dedupes in the store.

## Running a devcontainer in the browser (the Codespaces scenario)

This is the motivating scenario of Chapter 1: run a shared repository’s
Dev Container with no Docker daemon and no cloud VM, on a thin device.

1.  The operator opens the Platform Manager (cold-started from GitHub
    Pages) — the browser is now a peer that *is* the substrate (Law L1).

2.  They import the devcontainer: the repository’s `devcontainer.json`
    (validated against the Dev Container spec, `CC-4`) and its
    operating-system image become κ-addressed content, and the holospace
    is provisioned.

3.  The operator **launches** the holospace — a **workspace projection**
    (editor, file tree, terminal) opens in a new tab. The Boot Layer
    boots the holospace via the system-emulator codemodule on the
    browser’s `wasmi` `ContainerEngine` — the same lifecycle as a native
    or remote peer (boot, suspend to a κ snapshot, resume, migrate).

4.  The projection reads the environment’s content by κ and publishes
    the operator’s edits and commands as **canonical events** on the
    holospace’s channels — editing files and running a terminal, the
    Codespaces/Gitpod experience, entirely in the browser. Because state
    is a κ snapshot, the operator can suspend here and resume on another
    instance — local or remote — and the same holospace κ runs there too
    (Q6). The browser is a first-class compute substrate; the only
    limits are the device’s, not an account cap.

## Cold-start from GitHub Pages

1.  The browser loads a minimal loader from GitHub Pages and the
    **Hologram** platform’s κ.

2.  The loader resolves that κ and **verifies it by re-derivation** (Law
    L5); the gateway cannot forge content.

3.  The browser peer boots the Manager; from there everything is
    content-addressed.

## \<Runtime Scenario 1\>

*arc42 template structural anchor; the concrete runtime scenarios are
the named sections above.*

## \<Runtime Scenario 2\>

*arc42 template structural anchor; the concrete runtime scenarios are
the named sections above.*

## …​

*arc42 template structural anchor; the concrete runtime scenarios are
the named sections above.*

## \<Runtime Scenario n\>

*arc42 template structural anchor; the concrete runtime scenarios are
the named sections above.*
