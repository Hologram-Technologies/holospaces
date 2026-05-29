# Lifecycle: Organizational Project-Enabling Processes

## Life cycle model management

The project’s life-cycle model is this documentation system itself: arc42 + C4 (architecture), OPM ISO 19450 (conceptual model), and ISO/IEC/IEEE 15288 (this chapter), authored from `docs/src/` and validated by the V1–V8 pipeline (`docs/docs-definition.md`).

## Infrastructure management

Development and build infrastructure is declared as code: the devcontainer (toolchain), the documentation pipeline (`docs/scripts/`), and CI (`.github/workflows/`). The runtime infrastructure is the [hologram](https://github.com/Hologram-Technologies/hologram) substrate, consumed by reference — holospaces operates no infrastructure of its own (ADR-001).

## Portfolio management

holospaces is one repository in the UOR / Hologram portfolio (see [UOR-Foundation](https://github.com/UOR-Foundation) and [Hologram-Technologies](https://github.com/Hologram-Technologies)); it is scoped strictly to the boot/run/manage layer and defers platform-type and substrate concerns to their owning repositories (Chapter 3).

## Human resource management

holospaces is developed by UOR Foundation maintainers and contributors, including AI agents working under `AGENTS.md`; that file is the durable orientation any contributor reads first.

## Quality management

Quality is governed by the project’s laws (Chapter 2) and an external-ground-truth V&V discipline (Chapter 10, `docs/docs-definition.md`): the documentation is validated by V1–V8, and the implementation against imported external artifacts.

## Knowledge management

The authoritative knowledge of holospaces is this documentation; knowledge owned elsewhere (hologram, UOR-ADDR, Prism) is incorporated by hyperlink, never copied or assumed (Chapter 2).
