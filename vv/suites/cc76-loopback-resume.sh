#!/usr/bin/env bash
# CC-76 (core) — resume a warm .holo and render its app over the IN-TAB loopback bridge (no host socket).
# The exact sequence the browser open(κ) tab runs (X64Workspace wraps these x64::Cpu methods).
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if ! command -v cargo >/dev/null 2>&1; then echo "cc76-loopback-resume: SKIP — no cargo" >&2; exit 127; fi
cargo test --release --manifest-path "$ROOT/Cargo.toml" -p holospaces --features net \
    --test cc76_browser_loopback_resume a_warm_holo_renders_its_app_over_the_loopback_bridge -- --ignored --nocapture || exit 1
echo "cc76-loopback-resume: PASS"
