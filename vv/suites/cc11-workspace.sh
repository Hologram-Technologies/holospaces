#!/usr/bin/env bash
#
# CC-11 — the workspace projection renders and drives a running holospace (ADR-009)
#
# Component conformance suite, defined by arc42 chapter 10 (the Conformance
# catalog). The workspace projection (holospaces::projection) is the
# implementation under test; the authorities are:
#   • the substrate store + the reference σ-axis hashes (CC-3 / CC-1) — the
#     Editor/FS surface reads the environment content by κ and an edit advances
#     that κ;
#   • a real, running Linux terminal + the reference RISC-V machine
#     qemu-system-riscv64 as the differential oracle (vv/artifacts/cc11/) — the
#     Terminal/Intent surface drives an interactive shell, byte-identical to QEMU.
# Witness: crates/holospaces/tests/cc11_workspace.rs.

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
    echo "cc11-workspace: SKIP — cargo not available in this environment" >&2
    exit 127
fi

# The fast cargo-tier witness: the Editor/FS surface (read content by κ, edit
# advances the κ).
cargo test --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc11_workspace -- --nocapture || exit 1

# The Terminal surface: drive a real interactive Linux terminal. Boots Linux
# (~15 s), so it is #[ignore]d in the cargo tier and run here in release.
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc11_workspace the_workspace_drives_a_running_linux_terminal \
    -- --ignored --nocapture || exit 1

# The *raw interactive terminal* on the deployed devcontainer: raw keystrokes are
# echoed + line-edited by the guest tty, and Ctrl-C raises SIGINT in the
# foreground process (the controlling-terminal / signal contract a real terminal
# has). Boots the CC-22 BusyBox devcontainer (~release).
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test cc11_raw_terminal -- --ignored --nocapture || exit 1

# A *real-image* devcontainer (REAL_IMAGE_INIT) must stay up + interactive: the
# init runs the login shell with a controlling terminal in a respawn loop, so the
# guest does NOT halt after boot (the regression: exec'ing the shell as PID 1 let
# its exit panic the kernel — the devcontainer halted the instant it was used).
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces \
    --test real_image_init_stays_up -- --ignored --nocapture || exit 1

# The differential oracle, when the reference is available: the same interactive
# Image, fed input.txt after boot, must produce expected-session.txt on
# qemu-system-riscv64 (echo disabled so the session is deterministic). Re-derives
# the captured oracle live, so it is never stale.
CC11="$ROOT/vv/artifacts/cc11"
if command -v qemu-system-riscv64 >/dev/null 2>&1; then
    tmp="$(mktemp -d)"
    gzip -dc "$CC11/Image.gz" > "$tmp/Image"
    ( sleep 4; cat "$CC11/input.txt" ) | timeout 90 qemu-system-riscv64 \
        -M virt -m 128M -nographic -bios default -kernel "$tmp/Image" \
        -append "console=hvc0" 2>&1 | tr -d '\r' > "$tmp/qemu.log"
    sed -n '/HOLOSPACES-WORKSPACE-READY/,/HOLOSPACES-WORKSPACE-DONE/p' "$tmp/qemu.log" > "$tmp/session.txt"
    if diff "$tmp/session.txt" "$CC11/expected-session.txt" >/dev/null; then
        echo "cc11-workspace: qemu-system-riscv64 terminal differential PASS (oracle current)"
    else
        echo "cc11-workspace: qemu-system-riscv64 terminal differential FAILED — oracle drift" >&2
        rm -rf "$tmp"; exit 1
    fi
    rm -rf "$tmp"
else
    echo "cc11-workspace: qemu-system-riscv64 absent — differential pinned in expected-session.txt (captured from QEMU, per SOURCE.txt)"
fi
