#!/usr/bin/env bash
#
# CC-51 (LIVE) — The workbench's Source Control view reflects, commits, and pushes
#                the REAL repository over the holospace's own primitives
#
# OPM process: SD4 Working. The builtin SCM provider (holospace-scm) runs a real
# Git engine (isomorphic-git, κ-pinned) as NATIVE exec on the browser peer (the
# CC-48 discipline — heavy in-tab work is native peer exec, never the emulated
# guest) over the holospace's OWN virtio-9p workspace (CC-15) — the SAME `.git`
# content the guest's git reads (one content, Law L1). No server outside the
# holospace (Law L4).
#
# Authority: the VS Code SourceControl (`scm`) API + the Git on-disk object and
#   pack-protocol formats (the authority a commit's bytes re-derive against,
#   Law L5), with isomorphic-git as the spec-conformant engine; a real
#   `git http-backend` bare repo as the independent push oracle.
# Witnesses:
#   • host       crates/holospaces/tests/cc51_nested_workspace.rs — the host's
#     nested-path 9p API and a real busybox guest share one nested `.git`-shaped
#     tree over virtio-9p, both directions (CC-15 parity with the guest tree);
#   • deployed   crates/holospaces-web/web/scm-git-test.mjs — in Chromium the SCM
#     provider is live, reflects the real repo, a commit through the SCM input
#     RE-DERIVES to the canonical Git object (Law L5) + HEAD points at it, and a
#     PUSH lands in a real `git http-backend` bare repo (git log is the oracle).
#
# GREEN when both witnesses pass: nested 9p coherence (host) AND the deployed
#   provider's status + Law-L5-verified commit + a push received by a real remote.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEB="$ROOT/crates/holospaces-web/web"
VENDOR="$WEB/builtin-extensions/holospace-scm/vendor/isomorphic-git"

# Artifact-drift gate: the vendored Git engine must re-derive to its pinned
# sha256 (Law L5) — a tampered/updated bundle is refused (the extension verifies
# the same pins at load time).
( cd "$VENDOR" && sha256sum -c SHA256SUMS ) >/dev/null 2>&1 \
    || { echo "cc51-scm-git: artifact drift in $VENDOR/SHA256SUMS" >&2; exit 1; }

# (1) Host witness — the nested-path 9p workspace API at CC-15 parity with the
# guest tree (a real-OS boot; the substrate primitive the Git engine builds on).
# The host witness is a REQUIRED part of CC-51's gate: if cargo is absent we
# cannot run it, so SKIP the whole suite (exit 127) rather than continue and
# report a partial green from the deployed witness alone (no false green).
command -v cargo >/dev/null 2>&1 \
    || { echo "cc51-scm-git: SKIP — cargo absent (the host witness cannot run; refusing a partial green)"; exit 127; }
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc51_nested_workspace -- --ignored --nocapture \
    the_host_and_os_share_a_nested_workspace_tree_over_virtio_9p || exit 1

# (2) Deployed witness — the SCM provider in the real workbench (Chromium).
command -v node >/dev/null 2>&1 || { echo "cc51-scm-git: SKIP deployed witness — node absent"; exit 127; }
command -v git >/dev/null 2>&1 || { echo "cc51-scm-git: SKIP deployed witness — git (push oracle) absent"; exit 127; }
command -v wasm-pack >/dev/null 2>&1 || { echo "cc51-scm-git: SKIP deployed witness — wasm-pack absent"; exit 127; }
# Build the wasm peer (carries the nested-path ws_*_path bindings the SCM
# provider drives over 9p) so the witness runs against the product, not a stale
# bundle.
if [ ! -f "$WEB/pkg/holospaces_web_bg.wasm" ]; then
    "$ROOT/vv/lib/build-wasm-peer.sh" "$ROOT" || exit 1
fi
( cd "$WEB" && node scm-git-test.mjs ) || exit 1
exit 0
