#!/usr/bin/env bash
#
# CC-20 — a devcontainer provisions from a repository URL over the internet
#         (the import boundary; ADR-013)
#
# Component conformance suite (arc42 ch.10). holospaces fetches a repository's
# content by URL, reads its devcontainer.json (or applies a default image),
# pulls the devcontainer's OCI image from a registry via the OCI distribution
# protocol, and verifies every byte by re-derivation (a registry digest is a κ;
# Law L5). Witnessed HERMETICALLY here — a localhost server serves a pinned
# repository archive + the pinned CC-14 OCI image over the real distribution
# endpoints; the import client pulls + verifies + assembles + boots a real Linux
# that mounts the imported rootfs. Reproducible, no external network.
# (A live-internet smoke test against Docker Hub is `#[ignore]`d, not run here.)
# Witness: crates/holospaces/tests/cc20_import.rs (the `net` feature).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc20-import: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# Fast checks: default-image fallback + reference parsing (the `net` feature).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --test cc20_import a_repository_without_a_devcontainer_uses_the_default_image -- --nocapture || exit 1
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --lib import -- --nocapture || exit 1

# The hermetic end-to-end: import from a localhost repository URL → pull + verify
# the OCI image → assemble → boot a real Linux (release; a real-OS boot, ~18s).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net --release \
    --test cc20_import -- --ignored --nocapture \
    a_devcontainer_provisions_from_a_repository_url || exit 1
