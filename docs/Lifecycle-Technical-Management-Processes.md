# Lifecycle: Technical Management Processes

## Project planning

Work is documentation-driven: specifications and decisions precede code, and every implemented feature links back to the documentation it realizes (Chapter 2).

## Project assessment and control

Progress is assessed by the V&V gate (V1–V8 in CI) and git history rather than a narrative status document; a change is admitted only when its checks pass (Chapter 10).

## Decision management

Architectural decisions are recorded as ADRs (Chapter 9), each with status, context, decision, and consequences; implemented features cite the decision they realize.

## Risk management

Open risks and technical debts are tracked in Chapter 11; a resolved risk becomes a decision in Chapter 9 or is retired.

## Configuration management

Configuration identity is content addressing: every artifact is a κ-label over canonical bytes (Law L1), so a configuration is immutable and self-verifying. Source and dependency pins are managed in git, including the pinned `arc42-generator` submodule.

## Information management

Project information is the documentation corpus under `docs/`; rendered pages are generated from `docs/src/` and never hand-edited (`docs/docs-definition.md`).

## Measurement

The measurable gates are the V1–V8 validators plus the build’s determinism (idempotence) and page-size checks; performance budgets are measured in CI as they are defined.

## Quality assurance

QA is the V1–V8 documentation pipeline plus external-ground-truth implementation V&V — never self-reference (e.g. the native executor as the oracle for the browser engine; Chapter 10, `docs/docs-definition.md`).
