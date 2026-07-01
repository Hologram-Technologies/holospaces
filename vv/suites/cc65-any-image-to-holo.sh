#!/usr/bin/env bash
#
# CC-65 — any OCI image's REAL entrypoint runs on the x86-64 .holo core
#
# Component conformance suite (arc42 ch.10). The "run any docker image" promise needs the image's OWN
# Entrypoint/Cmd (with its Env/WorkingDir/User) to run as PID 1 — not a hardcoded shell. The keystone is
# a generic, libc-agnostic freestanding init (vv/artifacts/cc65/image-init, built from image-init.c) into
# which the host patches the image's run config (holospaces::image_init), so the init mounts the pseudo-fs
# and execve's the app DIRECTLY (no /bin/sh — distroless/scratch work too). The image's config is distilled
# by run_config_from_oci (Entrypoint++Cmd / Env / WorkingDir / numeric User). Everything rides the κ-disk
# (KappaBacking — every sector blake3, verify-on-receipt), so the bootable .holo is content-addressed L1/L5.
#
# Authority (the app's own observable behavior, which the emulator cannot fake):
#   image A — a real Alpine layer + OCI Cmd ["/bin/busybox","echo","HOLO-IMG-OK"] → the command's output.
#   image B — the image's Cmd runs a real nc SERVER that serves an HTTP body a real in-guest wget fetches
#             byte-exact (the TCP server surface CC-63 proved), proving "docker run <a server>".
#
# Witness: crates/holospaces/tests/cc65_any_image_to_holo.rs
#   :: image_a_alpine_cmd_runs_its_real_command, image_b_server_runs_and_serves_its_real_content
#   + the image_init unit tests (OCI-config parse, distroless named-user, patcher guards).
#
# Depends on: CC-45 (x64 Alpine boot + κ-disk), CC-63 (in-guest TCP), the compiled image-init template.
# Remaining CC-65 (not yet gated here): warm-snapshot a running app + CC-64 browser resume; host-reachable
#   via StdIngress (CC-60); live-registry nginx behind #[ignore]; `holo run <ref>` CLI.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc65-any-image-to-holo: SKIP — cargo not available in this environment" >&2
    exit 127
fi
if [ ! -f "$ROOT/vv/artifacts/cc65/image-init" ]; then
    echo "cc65-any-image-to-holo: SKIP — vv/artifacts/cc65/image-init not compiled (see image-init.c header)" >&2
    exit 127
fi

cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --test cc65_any_image_to_holo -- --nocapture || exit 1
# the library keystone (config parse + patcher)
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    image_init -- --nocapture || exit 1

echo "cc65-any-image-to-holo: PASS"
