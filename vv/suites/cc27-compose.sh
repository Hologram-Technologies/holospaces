#!/usr/bin/env bash
#
# CC-27 — a Docker Compose devcontainer resolves its service (ADR-011/016)
#
# holospaces reads the dockerComposeFile from the repository and resolves the
# devcontainer's `service` to its image (pulled, CC-20) or build (a Dockerfile
# build, CC-26) — never silently defaulting; a missing/ambiguous service is an
# explicit error. Authority: the Compose spec (services.<name>.{image|build}) +
# the Dev Container `dockerComposeFile`/`service`.
# Witness: crates/holospaces/tests/cc27_compose.rs + the compose unit tests.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc27-compose: SKIP — cargo unavailable" >&2; exit 127; fi

cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release --lib compose -- --nocapture || exit 1
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release --test cc27_compose -- --nocapture || exit 1
