# holospaces documentation

This directory is the project's normative specification — architecture, conceptual model, and lifecycle — authored under **arc42 + C4 / OPM ISO 19450 / ISO 15288** and rendered **in-repo**. See [docs-definition.md](docs-definition.md) for the full spec.

## Layout

- `src/` — **authored** sources (the only hand-edited docs): `arc42/adoc/`, `c4/`, `opm/`, `15288/`.
- `scripts/` — the build + validation pipeline (`install-tools.sh`, `validate.sh`, `build.sh`, validators `v1`–`v8`).
- `tools/` — pinned external-standard references (arc42 template pin, ISO 19450 OPL EBNF, ISO 15288 process list).
- `vendor/arc42-generator` — the arc42 → Markdown generator (git submodule).
- `*.md`, `images/` — **rendered** output (committed; do not hand-edit).

## Bootstrap

The devcontainer provides the prerequisites (JDK 21, Ruby 3, Node, Docker, cmake, build-essential, graphviz). The pipeline pins **JDK 21** — if the base image ships a newer JDK, rebuild the devcontainer first so the `java@21` feature is active. Then, from `docs/`:

    git submodule update --init --recursive   # if not already initialized
    scripts/install-tools.sh                  # download/build pinned tools; writes tools/versions.txt
    scripts/build.sh                          # validate (V1–V8) → render → stage → guard → idempotence

`scripts/validate.sh` runs the validators alone.

## Authoring

Edit the AsciiDoc / Structurizr DSL / OPL under `src/`, then run `scripts/build.sh`. Never edit the rendered `*.md` at the `docs/` root — they are regenerated on every build.
