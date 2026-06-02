#!/usr/bin/env bash
#
# CC-28 — the control plane reconfigures a running holospace over the substrate;
# configuration is content (ADR-018)
#
# A Codespaces/Gitpod (Ona) control panel reconfigures a RUNNING environment —
# lifecycle, storage, network, account/user. holospaces does this UOR-native: the
# panel produces a κ-addressed Configuration (embedding the issuing operator and
# the target instance), publishes it over the substrate, and the instance
# resolves it (verify-by-re-derivation, Law L5) and applies it — its state
# changes. No server, no control-plane→instance RPC.
#
# Authority/oracle: the substrate's content-addressing + sync contract (a real
# loopback HTTP content-addressed gateway, verify-on-receipt — the same model the
# e2e roster-sync witness uses) and, for the live network directive, a real host
# TcpListener bound on the running machine (the CC-21 ingress dual). The
# Codespaces/Gitpod control-panel reconfigure UX is the behavioural model.
# Witness: crates/holospaces/tests/cc28_reconfigure.rs —
#   * the control plane reconfigures an instance over the substrate (two peers,
#     real HTTP-CAS gateway, all four operation classes applied);
#   * reconfiguration is reproducible (same config ⇒ same κ) and authority-scoped
#     (wrong instance / unauthorized operator refused);
#   * a forwardPort directive modifies the RUNNING machine (a real, reachable
#     host listener, bound live — no reboot).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc28-reconfigure: SKIP — cargo unavailable" >&2; exit 127; fi
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc28_reconfigure -- --nocapture || exit 1
