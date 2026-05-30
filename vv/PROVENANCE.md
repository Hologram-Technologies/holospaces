# Imported validation artifacts — provenance

Every artifact holospaces is validated against is external and authoritative, imported and pinned. This file is the authoritative record of *what* is imported, *from where*, at *which pin*, and *how it is verified*. See the Conformance catalog in [arc42 chapter 10](../docs/src/arc42/adoc/10_quality_requirements.adoc) for which invariant each artifact validates.

## Live (specification conformance, `CS-*`)

These drive the documentation build (validators V1–V8) today.

| Artifact | Authority | Source | Pin | Verified by |
|---|---|---|---|---|
| arc42 template | arc42 (https://arc42.org) | `github.com/arc42/arc42-template` | SHA in `docs/tools/arc42-template-pin.txt` | checked out at the pin by `docs/scripts/install-tools.sh`; structural superset enforced by V1 |
| arc42-generator | arc42 | `github.com/arc42/arc42-generator` | submodule `docs/vendor/arc42-generator` (pinned commit) | git submodule pin; runs in V2 |
| OPM OPL grammar | ISO/PAS 19450:2015 (OPM) | transcribed EBNF, `docs/tools/iso-19450-opl.ebnf` | in-repo, pinned | V6 parses every `opl.txt` against it; V7 checks OPD↔OPL coherence |
| ISO 15288 process catalog | ISO/IEC/IEEE 15288 | process list, `docs/tools/iso-15288-processes.txt` | in-repo, pinned | V8 superset check |
| cmark-gfm | CommonMark / GFM | `github.com/github/cmark-gfm` | tag in `docs/tools/versions.txt`; sha256 in `docs/tools/checksums.txt` | built at the pinned tag by `install-tools.sh`; runs in V4 |
| github-markup | GitHub rendering | RubyGem (`docs/Gemfile.lock`) | locked version | runs in V5 |
| Structurizr | C4 (Structurizr DSL) | `download.structurizr.com` | version in `versions.txt`; sha256 in `checksums.txt` | checksum-verified on download by `install-tools.sh`; runs in V3 |

## Live (component conformance, `CC-*`)

Every component is implemented as a thin composition of the [hologram](https://github.com/Hologram-Technologies/hologram) substrate, consumed by reference (ADR-006) at the pinned rev `18f553d8578997ce32e7b653786a0bcf9b09a2c0` (git dependency, see the root `Cargo.toml`). Each `CC-*` row is witnessed against an external authority by its suite in `vv/suites/`, run by `vv/run.sh`.

Every artifact is external/authoritative — imported verbatim and pinned, never hand-authored. (This mirrors hologram's own conformance discipline, e.g. its `AS` class validates the σ-axis against the reference `blake3` crate, not against a vendored vector file.)

| Row | Authority | Source / pin | Verified by |
|---|---|---|---|
| `CC-1` κ-labels | the **reference implementations** of the σ-axis standards — the `blake3`, `sha2`, `sha3` crates (canonical BLAKE3 / FIPS 180-4 / FIPS 202 / Keccak), each tested against the published vectors upstream | the crates pinned in `Cargo.lock` (no vendored vectors) | `cc1-kappa-addressing.sh` → `tests/cc1_kappa_kat.rs`: holospaces' κ-label digest equals the reference implementation byte-for-byte across all five axes and across chunk/subtree boundaries (hologram `AS` pattern); determinism, single-bit sensitivity, and re-derivation (Law L5) |
| `CC-2` `.holo` engine | the native hologram `.holo` executor (`hologram-exec`) as oracle | `Hologram-Technologies/hologram` (pinned rev) | `cc2-holo-engine.sh` → `tests/cc2_holo_engine.rs`: identical `.holo` yields identical κ across independent builds and across the byte- and address-boundary surfaces (determinism + content-addressing). **Live browser differential:** `scripts/browser-manager-test.sh` (CI `browser` job) runs the same `.holo` through the executor compiled to wasm in headless Chromium and asserts an identical output κ to native — the browser engine equals the native one (arc42 ch.11 RT2, realized) |
| `CC-3` peer storage | the hologram substrate conformance battery (TCK) | `hologram-substrate-tck` (pinned rev) | `cc3-substrate-tck.sh` → `tests/cc3_substrate_tck.rs`: `store_battery` run against the stores holospaces resolves through (`hologram-store-mem`, `hologram-store-native`) |
| `CC-4` devcontainer holospace | the **published Dev Container JSON Schema** + **real authoritative `devcontainer.json` configs** | `vv/artifacts/cc4/devContainer.base.schema.json` (`devcontainers/spec@c95ffeed`, sha256 `a0883c04…`); `*.devcontainer.json` (`devcontainers/templates@b3645066`); pins in `vv/artifacts/cc4/SOURCE.txt` | `cc4-devcontainer.sh` → `tests/cc4_devcontainer.rs`: every real template config validates against the schema (the schema is the judge) and the ingestor accepts it; the schema rejects a non-conformant config; same source ⇒ same κ (Q4). The repo's features-only config ingests as the documented default-image superset |
| `CC-5` Wasm modules | the **WebAssembly specification's own `test/core` conformance suite** | `vv/artifacts/cc5/{func,binary}.wast` imported verbatim from `WebAssembly/spec@93c7fab` (commit in `vv/artifacts/cc5/SOURCE-COMMIT.txt`) | `cc5-wasm.sh` → `tests/cc5_wasm.rs`: holospaces' validator agrees with the spec's own `module` / `assert_invalid` / `assert_malformed` directives; the substrate's closed host surface (spec §4.4) refuses non-`hologram` imports |
