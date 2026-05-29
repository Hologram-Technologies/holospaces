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

| Row | Authority | Source / pin | Verified by |
|---|---|---|---|
| `CC-1` κ-labels | BLAKE3 + FIPS 180-4 (SHA-2) + FIPS 202 (SHA-3) + Keccak published test vectors | `vv/artifacts/cc1/hash-kats.json`, sha256 `34db64e7b06817634e7aeafe3b75d5b2f912fadb95cc7921bd3b59bc7d1dce90` (standards cited per-axis inside) | `cc1-kappa-addressing.sh` → `tests/cc1_kappa_kat.rs`: holospaces' κ-label digests (minted through the substrate) equal the published vectors directly (Law L5); the reference hash crates independently reproduce them |
| `CC-2` `.holo` engine | the native hologram `.holo` executor (`hologram-exec`) as oracle | `Hologram-Technologies/hologram` (pinned rev) | `cc2-holo-engine.sh` → `tests/cc2_holo_engine.rs`: identical `.holo` yields identical κ across independent builds and across the byte- and address-boundary surfaces (determinism + content-addressing). The live browser-vs-native cross-host run is the browser peer's Playwright harness (hologram `store-opfs`), sharing this `address_bytes` σ-axis |
| `CC-3` peer storage | the hologram substrate conformance battery (TCK) | `hologram-substrate-tck` (pinned rev) | `cc3-substrate-tck.sh` → `tests/cc3_substrate_tck.rs`: `store_battery` run against the stores holospaces resolves through (`hologram-store-mem`, `hologram-store-native`) |
| `CC-4` devcontainer holospace | the Dev Container specification + the OCI image specification | `vv/artifacts/cc4/devcontainer-cases.json`, sha256 `244975212fccf4abc02823005246fdd5a7e78ef192456f62274c28b9fcb65f95` (`containers.dev`; OCI) | `cc4-devcontainer.sh` → `tests/cc4_devcontainer.rs`: the ingestor's accept/reject matches the spec on every case (incl. this repo's real `.devcontainer/devcontainer.json`); same source ⇒ same κ (Q4) |
| `CC-5` Wasm modules | the WebAssembly specification (and its `test/core` suite) | `vv/artifacts/cc5/wasm-cases.json`, sha256 `76c115137aad874e052fc9d1a197586becb4809210e4c76d97df0c45577dc384` (`webassembly.org`; `WebAssembly/spec`) | `cc5-wasm.sh` → `tests/cc5_wasm.rs`: holospaces' validator accepts exactly the spec-valid modules and rejects the invalid ones; the substrate's closed host surface (spec §4.4) refuses non-`hologram` imports |
