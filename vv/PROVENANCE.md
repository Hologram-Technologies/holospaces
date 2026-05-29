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

## To import with the component they validate (component conformance, `CC-*`)

Each is the external authority for a component not yet implemented. It is imported (with its pin + checksum recorded here, content-verified) **at the same time** as the component, and the witness is added to `vv/run.sh`.

| Will validate | Authority to import | Source |
|---|---|---|
| `CC-1` κ-labels | BLAKE3 + FIPS 180-4 (SHA-2) + FIPS 202 (SHA-3) + Keccak test vectors | the BLAKE3 reference repo; NIST CAVS |
| `CC-2` browser `.holo` engine | native `hologram` executor outputs (differential oracle) | `github.com/Hologram-Technologies/hologram` |
| `CC-3` peer storage | `hologram` substrate conformance battery (TCK) | `github.com/Hologram-Technologies/hologram` |
| `CC-4` devcontainer holospace | Dev Container spec + OCI image spec | `containers.dev`; OCI |
| `CC-5` Wasm modules | WebAssembly spec test suite | `github.com/WebAssembly/spec` |
