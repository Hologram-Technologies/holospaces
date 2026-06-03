# holospaces

A UOR-native **boot layer** over the [hologram](https://github.com/Hologram-Technologies/hologram) substrate. holospaces provisions and runs **holospaces** — bootable, content-addressed environments, from a single compute artifact to a full Linux development environment — and ships **Hologram**, the first-party holospace whose console (the **Platform Manager**) manages the rest. For the development-environment use case it is a UOR-native, serverless Gitpod/Codespaces.

## Where things are

- **[`docs/`](docs/)** — the **authoritative specification** (architecture, conceptual model, lifecycle), authored under arc42 + C4 / OPM ISO 19450 / ISO/IEC/IEEE 15288. Start at [`docs/docs-definition.md`](docs/docs-definition.md). The rendered pages are generated — do not hand-edit them.
- **[`AGENTS.md`](AGENTS.md)** — timeless orientation for contributors and agents.
- **[`vv/`](vv/)** — the **Verification & Validation** framework: holospaces evaluated against external authoritative standards, defined by the docs (arc42 chapter 10).

## Build & validate

The devcontainer provides the toolchain (Rust, Lean, wasm-pack; and for the docs: JDK 21, Ruby, Node, Docker, cmake, graphviz, QEMU). Its post-create hook provisions the pinned docs toolchain; rerun this manually after docs toolchain changes:

    docs/scripts/install-tools.sh   # or: just install-tools

Then:

    just docs   # build + validate the documentation (validators V1–V8)
    just vv     # run the full V&V

## Conventions

The documentation is authoritative; every implemented feature links back to it, and nothing is "complete" until its conformance row in [`vv/`](vv/) is witnessed against its external authority. External systems (hologram, UOR-ADDR, Prism) are referenced by hyperlink, never restated. There is no separate wiki and no narrative status file — implementation status lives in git and CI.
