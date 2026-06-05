# vv/ — Verification & Validation

The **executable V&V framework** for holospaces. It evaluates holospaces and its parts against **external authoritative specifications and standards** — never against itself.

- **Authoritative definition:** the documentation, [arc42 chapter 10](../docs/src/arc42/adoc/10_quality_requirements.adoc) ("Verification and Validation" + the **Conformance catalog**). This directory *implements* that definition; the docs are the source of truth.
- **Imported artifacts + provenance:** [PROVENANCE.md](PROVENANCE.md) records every real external validation artifact, its source, its pin (version/SHA), and how it is verified. Imported artifacts are content-addressed and verified on import.
- **Runner:** `./run.sh` is the single V&V entry point (also `just vv`).

## Tiers

- **Specification conformance (`CS-*`)** — the holospaces specification (the documentation) validated against arc42 + C4, OPM ISO 19450, and ISO/IEC/IEEE 15288, via validators V1–V8. **Live on every build.**
- **Component conformance (`CC-*`)** — each implemented component validated against the external authority for its behavior (hash KATs for κ-addressing, the native executor as oracle for the browser engine, the substrate TCK for storage, the Dev Container/OCI specs, the WebAssembly spec). **Conformance-driven:** each authority is defined now; the witness is added with the component, never by self-reference. Live witnesses live in [`suites/`](suites/).
- **Targets (`CC-*`, behavior-driven)** — unfinished work, with its **behavioral V&V written first**. Each `CC-*` row marked *target* in the catalog has an executable, **expected-RED** suite in [`targets/`](targets/) that defines "done" before any implementation, realizing a process in the OPM conceptual model. This tier is **non-gating**: a RED target never fails V&V and never blocks deploy. Build the component to its target until the suite is green, then **promote** it: move the suite `targets/ → suites/`, un-`#[ignore]` its witness, and turn the catalog row `live`. A GREEN suite left in `targets/` is a placement defect (its component is live). `./run.sh` lists the targets so an agent can see the remaining work at a glance.

A component is "correctly implemented and complete" only when its `CC-*` row is witnessed against its external authority. **To find what is left to build, read the *target* rows (chapter 10) and the suites in [`targets/`](targets/).**
