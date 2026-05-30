# Runtime View

The runtime scenarios below show how holospaces behaves over time. (The
arc42 template placeholder scenario headings are retained at the end for
structural conformance.)

## Provisioning a holospace from a devcontainer

1.  The operator gives the Manager a link to a git repository containing
    a valid `devcontainer.json`.

2.  holospaces ingests the repository and the devcontainer config at the
    boundary and **κ-addresses** them into the store; the config
    *selects* a κ-addressed Wasm-recompiled userland for the Linux/POSIX
    surface (ADR-008), not an OCI image by location (shared modules
    dedupe; Laws L1/L2).

3.  It composes a **holospace definition** — a κ over those parts; the
    same `devcontainer.json` yields the same κ (reproducibility, Q4).

4.  The Boot Layer spawns the holospace through the substrate’s runtime
    as a hologram-native Wasm container providing the environment; the
    userland binds only the host ABI and I/O is expressed over the
    content-addressed graph.

5.  The holospace appears in the Manager, ready to use.

## Provisioning a holospace from a holo-file

1.  The operator supplies a `.holo` artifact (e.g. a model compiled by
    [hologram-ai](https://github.com/Hologram-Technologies/hologram-ai))
    by its κ — reproducible by definition, since the κ *is* the
    artifact’s content address (Q4).

2.  The Boot Layer resolves the κ (locally, else fetched and verified,
    Law L5) and runs it via the **.holo Engine** over the
    [hologram](https://github.com/Hologram-Technologies/hologram)
    executor.

3.  Lifecycle is identical to any other holospace.

## Suspend, resume, and migrate

1.  The operator suspends a running holospace; its state is captured as
    a **κ snapshot** (state is content).

2.  Because the snapshot is a κ, any of the operator’s instances that
    can resolve it may **resume** it — including a different
    environment.

3.  This makes migration a content operation: suspend on one peer,
    resume on another; unchanged state dedupes in the store.

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
