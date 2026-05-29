# vv/ — Verification & Validation

The **executable V&V framework** for holospaces. It evaluates holospaces and its parts against **external authoritative specifications and standards** — never against itself.

- **Authoritative definition:** the documentation, [arc42 chapter 10](../docs/src/arc42/adoc/10_quality_requirements.adoc) ("Verification and Validation" + the **Conformance catalog**). This directory *implements* that definition; the docs are the source of truth.
- **Imported artifacts + provenance:** [PROVENANCE.md](PROVENANCE.md) records every real external validation artifact, its source, its pin (version/SHA), and how it is verified. Imported artifacts are content-addressed and verified on import.
- **Runner:** `./run.sh` is the single V&V entry point (also `just vv`).

## Tiers

- **Specification conformance (`CS-*`)** — the holospaces specification (the documentation) validated against arc42 + C4, OPM ISO 19450, and ISO/IEC/IEEE 15288, via validators V1–V8. **Live on every build.**
- **Component conformance (`CC-*`)** — each implemented component validated against the external authority for its behavior (hash KATs for κ-addressing, the native executor as oracle for the browser engine, the substrate TCK for storage, the Dev Container/OCI specs, the WebAssembly spec). **Conformance-driven:** each authority is defined now; the witness is added with the component, never by self-reference.

A component is "correctly implemented and complete" only when its `CC-*` row is witnessed against its external authority.
