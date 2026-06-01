#!/usr/bin/env bash
#
# CC-24 — the devcontainer authenticates with GitHub (and other services) over
# the holospaces network (ADR-017)
#
# A Codespace/Gitpod lets you sign in to GitHub and other services. holospaces
# does NOT intermediate that auth — it provides the network (CC-16), and the
# devcontainer's tools authenticate over it using the service's own published
# OAuth flow: the OAuth 2.0 Device Authorization Grant (RFC 8628), which needs no
# backend secret and so works from a browser/Pages deployment. The token lives in
# the devcontainer, never in holospaces (content-blind; Laws L1/L3).
# Authority: RFC 8628; oracle: a hermetic GitHub-shaped device-flow server.
# Witness: crates/holospaces/tests/cc24_auth.rs (release; a real-OS boot, ~23s).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc24-auth: SKIP — cargo unavailable" >&2; exit 127; fi
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --release \
    --test cc24_auth -- --ignored --nocapture \
    the_devcontainer_authenticates_with_github_over_the_network || exit 1
