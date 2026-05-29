# holospaces Documentation — Definition

> Normative specification for holospaces' in-repo documentation system. **Timeless**: it defines structure, standards, and build discipline — not status.

## What this is

The holospaces documentation is a **normative specification** of the project, authored under three coordinated external standards and kept **entirely in this repository** (no separate GitHub wiki):

- **arc42 + C4** — architecture (12 chapters; ADRs in chapter 09).
- **OPM (ISO 19450)** — the conceptual model (bimodal: Object-Process Diagrams + Object-Process Language).
- **ISO/IEC/IEEE 15288** — the system lifecycle.

The pipeline is ported from the UOR-Framework wiki's documentation system and adapted to render **in-repo** (into `docs/`) rather than to a `.wiki.git` root. The implementation is accountable to these documents, not the reverse.

## Authoritative external specifications

The documentation **authors none** of these standards — it conforms to them (the same "external ground truth" discipline as code V&V):

| Concern | Authority |
|---|---|
| arc42 structure (12 chapters) | `arc42/arc42-template`, pinned in `tools/arc42-template-pin.txt` |
| arc42 → Markdown | `arc42/arc42-generator` (submodule `vendor/arc42-generator`) |
| C4 model | Structurizr DSL (`structurizr.war`) |
| Conceptual model | ISO 19450 OPM/OPL (grammar in `tools/iso-19450-opl.ebnf`) |
| Lifecycle | ISO/IEC/IEEE 15288 (`tools/iso-15288-processes.txt`) |
| GFM rendering | cmark-gfm + GitHub-markup |

## Authored vs. rendered

**Authored** — the only hand-edited docs, under `src/`:
- `src/arc42/adoc/NN_*.adoc` — the 12 chapters (+ `arc42-template.adoc`)
- `src/c4/workspace.dsl` — the C4 model
- `src/opm/SD*/opl.txt` (+ per-OPD `opd.svg`) — the conceptual model
- `src/15288/*.adoc` — lifecycle processes

**Rendered** — committed, **never hand-edited** (overwritten by every build), at the `docs/` root:
- `NN-Chapter-Name.md` (arc42), `Conceptual-Model.md` (OPM), `Lifecycle-*.md` (15288), `images/*.svg`

Hand-editing rendered output is pointless: `scripts/build.sh` regenerates it.

## Build & validation

- `scripts/install-tools.sh` — provision the pinned toolchain; writes `tools/versions.txt`.
- `scripts/validate.sh` — run validators **V1–V8** (arc42 structure, Structurizr, CommonMark/GFM, GitHub-markup, OPL syntax, OPD↔OPL coherence, ISO 15288 superset).
- `scripts/build.sh` — pre-flight version pins → validate → render → stage into `docs/` → page-size guard → idempotence check.

A change is correct only when `build.sh` passes **and** the rendered output is byte-identical across two clean builds (Stage 5).

## In-repo retarget (vs. the source wiki)

Every script roots itself at the parent of `scripts/`, so the whole system lives under `docs/` and stages rendered Markdown into `docs/` itself. There is no `.wiki.git`: the rendered Markdown is committed in-repo, and CI publishes nothing to a wiki.
