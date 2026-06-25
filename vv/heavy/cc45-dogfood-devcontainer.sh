#!/usr/bin/env bash
# CC-45 DOGFOOD — holospaces builds in its OWN, unmodified devcontainer.
#
# The #13 reference, end to end and for real: this repo's `.devcontainer` (Ubuntu
# 24.04 + the unmodified toolchain + its ghcr features) is built by the **Dev
# Container CLI** exactly as Codespaces/Gitpod would, its rootfs is exported, and the
# `cc45_dogfood` witness boots it on the holospaces **x86-64 core** and has the real
# **gcc 13.3** (cc1 → as → ld over glibc 2.39 + ld.so) compile a C program in-guest —
# then runs the binary it built (DOGFOOD-GCC-BUILT:42).
#
# The rootfs is multi-GiB, so it is NOT a committed fixture — it is built here. Needs
# docker + node + network for the base/features, AND ~24 GiB RAM (a full Ubuntu+gcc
# boot holds the κ-disk content + the guest's copy-on-write sectors in memory).
#
# Status: HEAVY (ON-DEMAND) — this lives in vv/heavy/, NOT vv/suites/, so the per-push
#   V&V gate does NOT run it: the build is ~30 min and the boot needs ~24 GiB RAM,
#   which OOMs the free 16 GiB CI runner. It is a REAL, reproducible validation (no
#   fixture, no stub, no skip) — run it on a machine with the RAM:
#       bash vv/heavy/cc45-dogfood-devcontainer.sh
#   The per-push gate proves #13's build-capability with the FAST in-guest witnesses
#   (the cc45 suite: a static TinyCC compiles + runs a program in-guest; a real OCI
#   image boots; the Dockerfile/features pipelines feed the boot). THIS suite is the
#   full-fat dogfood — the actual repo devcontainer + the real gcc 13.3.

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMG="holospaces-dogfood:cc45"
CTR="holospaces-dogfood-cc45"
ROOTFS="$(mktemp -d)/devcontainer-rootfs.tar"

command -v docker >/dev/null 2>&1 || { echo "cc45-dogfood: docker is required to build the devcontainer" >&2; exit 1; }
command -v npx >/dev/null 2>&1 || { echo "cc45-dogfood: node/npx is required for the Dev Container CLI" >&2; exit 1; }

cleanup() {
    docker rm -f "$CTR" >/dev/null 2>&1 || true
    docker rmi -f "$IMG" >/dev/null 2>&1 || true
    rm -f "$ROOTFS" 2>/dev/null || true
}
trap cleanup EXIT

# A 4.4 GB devcontainer image + a 4.2 GB rootfs export + a transient ext4 image need
# headroom; on a CI runner reclaim the large preinstalled toolchains we don't use
# (best-effort, only what's present). Harmless locally (the dirs usually don't exist).
if [ -n "${CI:-}" ]; then
    for d in /usr/share/dotnet /usr/local/lib/android /opt/ghc \
             /opt/hostedtoolcache/CodeQL /usr/local/.ghcup; do
        [ -d "$d" ] && sudo rm -rf "$d" 2>/dev/null || true
    done
fi

echo "== [1/3] build THIS repo's devcontainer, unmodified (Dev Container CLI) =="
# Exactly the Codespaces/Gitpod resolution: the Dockerfile + every ghcr feature.
npx --yes @devcontainers/cli@0.87.0 build \
    --workspace-folder "$ROOT" --image-name "$IMG" >/dev/null 2>&1 \
    || { echo "cc45-dogfood: devcontainer build failed" >&2; exit 1; }
echo "built $IMG ($(docker image inspect "$IMG" --format '{{.Size}}' 2>/dev/null) bytes)"

echo "== [2/3] export its rootfs =="
docker create --name "$CTR" "$IMG" true >/dev/null 2>&1 \
    || { echo "cc45-dogfood: docker create failed" >&2; exit 1; }
docker export "$CTR" -o "$ROOTFS" \
    || { echo "cc45-dogfood: docker export failed" >&2; exit 1; }
echo "exported rootfs: $(du -h "$ROOTFS" | cut -f1)"

echo "== [3/3] boot it on the x86-64 core + compile in-guest =="
CC45_DOGFOOD_ROOTFS="$ROOTFS" cargo test --release --manifest-path "$ROOT/Cargo.toml" \
    -p holospaces --test cc45_dogfood holospaces_builds_in_its_own_real_devcontainer \
    -- --ignored --nocapture \
    || { echo "cc45-dogfood: the real devcontainer did not build a program in-guest" >&2; exit 1; }

echo "cc45-dogfood: PASS — holospaces built a program in its own unmodified devcontainer (gcc 13.3, in-guest, on the x86-64 core)"
