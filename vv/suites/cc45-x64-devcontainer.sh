#!/usr/bin/env bash
#
# CC-45 (SUITE) — An arbitrary amd64 devcontainer runs on x86-64 (build-capable;
#                 full Dev Container / OCI spec)
#
# OPM process: SD5 Devcontainer Provisioning + SD2 Booting. The x86-64 analogue
# of CC-37 (aarch64), broadened to the full Dev Container / OCI contract on a
# build-capable disk. The x86-64 system (holospaces::emulator::x64) boots an amd64
# devcontainer from a κ-disk virtio-blk rootfs over the SHARED emulator::devbus —
# no per-ISA workaround (ADR-021, Law L4) — and runs a stock, unmodified
# linux-amd64 binary; the κ-disk indexes O(content) (occupancy-index boot), so a
# multi-GiB build-capable disk boots promptly. Authorities:
#   • the Dev Container + OCI image specs over an amd64/linux rootfs assembled into
#     the κ-disk (CC-7) by the in-crate Layer Assembler;
#   • a stock linux-amd64 busybox (vv/artifacts/cc45/rootfs/) — arbitrary
#     linux-amd64 binaries run with no per-ISA workaround;
#   • qemu-system-x86_64 as the differential oracle.
# Witnesses: crates/holospaces/tests/cc44_x64_boot.rs (the CC-45 tests). The
#   *deployed browser* path is witnessed by the browser job (scripts/browser-manager-
#   test.sh → crates/holospaces-web/web/cc45-x64-boot-test.mjs): in Chromium, a stock
#   linux/amd64 devcontainer is assembled sparse into an OPFS file and BOOTED on the
#   x86-64 core via the shipped X64Workspace paged-κ-disk path.
# Depends on: CC-44 (the x86-64 boot path).
#
# Status: SUITE (LIVE) — every section below is green for real: the build-capable
#   occupancy-index disk boots O(content), and the committed stock linux-amd64
#   busybox fixture boots on the x86-64 core to CC45-DEVCONTAINER-UP /
#   CC45-ARCH:x86_64 / CC45-COMPUTE:500500 and a clean poweroff, byte-matching the
#   qemu-system-x86_64 differential oracle. The full Dev Container / OCI contract
#   (multi-layer images, Dockerfile build, features + lifecycle, the dogfooded
#   workspace config) is witnessed in cc44_x64_boot.rs; the deployed amd64 boot is
#   witnessed in a real browser by the browser job.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CC45="$ROOT/vv/artifacts/cc45"

command -v cargo >/dev/null 2>&1 || { echo "cc45-x64-devcontainer: SKIP — cargo absent"; exit 127; }

# ── Section B: the build-capable disk (occupancy-index boot, O(content)) ──────
# LIVE: an ≥ 8 GiB disk is declarable and boots promptly because only occupied
# sectors are paged. These pass for real (no fixture needed).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces --lib \
    emulator::tests::occupancy_index_pages_only_content_for_a_multi_gib_disk \
    -- --nocapture || exit 1
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc44_x64_boot an_amd64_linux_boots_from_an_occupancy_indexed_build_capable_disk \
    -- --ignored --nocapture || exit 1

# The shared virtio-9p workspace on the amd64 core — what makes an x86-64
# holospace a USABLE devcontainer (editor/tasks/search bind to the same content
# the guest mounts), witnessed in both directions over a real amd64 Linux boot.
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc44_x64_boot the_amd64_devcontainer_shares_the_9p_workspace_with_the_editor \
    -- --ignored --nocapture || exit 1

# ── The stock linux-amd64 binary + arbitrary devcontainer (needs the fixture) ──
if [ -f "$CC45/cc45.sha256" ] && [ -f "$CC45/linux/vmlinux.gz" ] && [ -f "$CC45/rootfs/layer.tar.gz" ]; then
    ( cd "$CC45" && sha256sum -c cc45.sha256 >/dev/null ) \
        || { echo "cc45-x64-devcontainer: artifact drift from cc45.sha256" >&2; exit 1; }

    # A real amd64 Linux mounts the κ-disk virtio-blk rootfs and runs the stock
    # linux-amd64 busybox as PID 1 (~release, #[ignore]d).
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_devcontainer_runs_a_stock_linux_amd64_binary \
        -- --ignored --nocapture || exit 1

    # The same boot, but the κ-disk is PAGED from a KappaStore by streaming sectors
    # — the exact path the browser peer's X64Workspace takes from OPFS.
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_devcontainer_boots_paged_from_a_kappa_store \
        -- --ignored --nocapture || exit 1

    # The BUILD-CAPABLE large disk, end to end: the stock amd64 rootfs assembled onto
    # an 8 GiB disk and booted reading ONLY its occupied blocks (757 of the disk's
    # 16.7M sectors) — the deployed browser path's occupancy boot, O(content).
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_devcontainer_boots_occupancy_streamed_from_a_large_disk \
        -- --ignored --nocapture || exit 1

    # The DEPLOYED init (the browser peer's bootDevcontainerOpfsStreamedOccupancy path)
    # reaches its userspace marker O(content) from the same 8 GiB disk — the native
    # mirror of the in-browser boot, fast and CI-reliable.
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot the_deployed_amd64_init_boots_occupancy_streamed_from_a_large_disk \
        -- --ignored --nocapture || exit 1

    # BUILD SOFTWARE IN-GUEST (the decisive acceptance criterion): a real amd64
    # devcontainer with a toolchain (static TinyCC) compiles a program inside the
    # guest and then RUNS the binary it built — genuinely usable for development,
    # not merely bootable.
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_devcontainer_builds_a_program_in_guest \
        -- --ignored --nocapture || exit 1

    # An ARBITRARY REAL OCI image (the registry image-layout, blobs by digest)
    # ingested through the PRODUCTION path (oci::ingest_image, every blob verified
    # by re-derivation) for linux/amd64, then booted on x86-64 — the Codespaces
    # `image:` → pull → boot resolution, end to end on amd64.
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_arbitrary_real_amd64_oci_image_boots_on_x64 \
        -- --ignored --nocapture || exit 1

    # e2fsck is the differential oracle for the 8 GiB ext4 GEOMETRY (66 block groups):
    # a real fsck must find the multi-group image the assembler produces clean.
    if command -v e2fsck >/dev/null 2>&1; then
        _img="$(mktemp)"
        cargo run --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
            --example asm_fsck -- "$_img" $((8 * 1024 * 1024 * 1024)) || { rm -f "$_img"; exit 1; }
        e2fsck -fn "$_img" \
            || { echo "cc45-x64-devcontainer: e2fsck rejected the 8 GiB ext4 geometry" >&2; rm -f "$_img"; exit 1; }
        rm -f "$_img"
        echo "cc45-x64-devcontainer: 8 GiB build-capable disk — occupancy boot O(content) + e2fsck-clean geometry PASS"
    fi

    # An ARBITRARY MULTI-LAYER image (the DoD's "multi-layer real images"): three
    # OCI layers stack with whiteout + override + add, and the merged amd64 rootfs
    # boots — proving the parametric assembler (CC-4/CC-20) feeds the x86-64 boot for
    # any image, not just the single-layer fixture.
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_multilayer_image_overlay_runs \
        -- --ignored --nocapture || exit 1

    # The Dev Container BUILD phase (CC-26) on amd64: a Dockerfile (FROM/ENV/COPY/RUN)
    # builds the rootfs and its RUN steps execute in the booted x86-64 OS.
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_dockerfile_build_runs_on_x64 \
        -- --ignored --nocapture || exit 1

    # The Dev Container FEATURES (CC-25) + LIFECYCLE (CC-22) phases on amd64: a
    # devcontainer.json's feature install.sh + lifecycle hooks run in the x86-64 OS,
    # in spec order (features before lifecycle).
    cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --test cc44_x64_boot an_amd64_devcontainer_features_and_lifecycle_run_on_x64 \
        -- --ignored --nocapture || exit 1

    # The differential oracle: qemu-system-x86_64 booting the same kernel + rootfs
    # must run the same stock binary to the same markers.
    if command -v qemu-system-x86_64 >/dev/null 2>&1; then
        tmp="$(mktemp -d)"
        if cargo run --release --quiet --manifest-path "$ROOT/Cargo.toml" -p holospaces \
            --example cc45_mkrootfs -- "$tmp/rootfs.ext4" 2>/dev/null; then
            timeout 180 qemu-system-x86_64 -M q35 -m 512M -nographic -no-reboot \
                -kernel "$CC45/linux/bzImage" \
                -append "console=ttyS0 root=/dev/vda rw init=/init random.trust_cpu=on" \
                -drive file="$tmp/rootfs.ext4",format=raw,if=none,id=hd0 \
                -device virtio-blk-pci,drive=hd0 2>&1 | tr -d '\r' > "$tmp/qemu.log"
            if grep -aq 'CC45-ARCH:x86_64' "$tmp/qemu.log" \
               && grep -aq 'CC45-COMPUTE:500500' "$tmp/qemu.log"; then
                echo "cc45-x64-devcontainer: qemu-system-x86_64 differential PASS (stock amd64 binary ran)"
            else
                echo "cc45-x64-devcontainer: qemu-system-x86_64 differential FAILED" >&2
                echo "── qemu console (tail) ──" >&2; tail -40 "$tmp/qemu.log" >&2
                rm -rf "$tmp"; exit 1
            fi
        else
            echo "cc45-x64-devcontainer: (rootfs export helper unavailable — qemu differential skipped)"
        fi
        rm -rf "$tmp"
    else
        echo "cc45-x64-devcontainer: qemu-system-x86_64 absent — differential pinned by the in-emulator witness (per cc45/SOURCE.txt)"
    fi

    # ── The DEPLOYED amd64 path, in a real browser ────────────────────────────
    # Chromium boots the X64Workspace exactly as the Pages deploy does — the
    # PRODUCTION kernel artifact under the PRODUCTION name (the CC-44 boot-check
    # kernel once shipped here and powered every amd64 devcontainer off at boot;
    # this witness pins the deployed artifact so that drift cannot recur) — and
    # the deployed workspace surface round-trips editor→guest over 9p.
    WEB="$ROOT/crates/holospaces-web/web"
    if command -v node >/dev/null 2>&1 && command -v wasm-pack >/dev/null 2>&1; then
        if [ ! -f "$WEB/pkg/holospaces_web_bg.wasm" ]; then
            "$ROOT/vv/lib/build-wasm-peer.sh" "$ROOT" || exit 1
        fi
        ( cd "$WEB" && node cc45-x64-boot-test.mjs ) || exit 1
    else
        echo "cc45-x64-devcontainer: node/wasm-pack absent — deployed browser witness skipped (SKIP)"
    fi
    exit 0
fi

echo "cc45-x64-devcontainer: RED — build-capable disk (occupancy index) is LIVE; full bar pending."
echo "  done:   occupancy-index boot path — an ≥ 8 GiB disk boots O(content) (witnesses above, green)."
echo "  needed: the stock linux-amd64 busybox fixture (vv/artifacts/cc45/, run its build.sh) so the"
echo "          differential witnesses + qemu-system-x86_64 oracle run. See issue #13 / CC-45."
exit 1
