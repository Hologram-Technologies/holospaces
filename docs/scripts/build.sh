#!/usr/bin/env bash
#
# build.sh — top-level entry point: runs Stages 1..5 to produce the
# committable artifacts at the wiki root.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly VERSIONS_FILE="$REPO_ROOT/tools/versions.txt"

err()  { printf 'build: ERROR: %s\n' "$*" >&2; }
info() { printf 'build: %s\n' "$*"; }

cd "$REPO_ROOT"

# ============================================================================
# Stage 0 — Clear transient build caches
# ============================================================================
# Force a clean rebuild on every invocation. The arc42-generator's gradle/
# docker pipeline caches per-chapter MD outputs in vendor/arc42-generator/
# build/; the top-level build/ dir caches stage outputs (wiki-frame, 15288,
# OPM, scientific). Both have historically retained stale outputs when the
# source AsciiDoc changed in ways the incremental-build cache didn't
# recognize. Nuking both at Stage 0 guarantees every run reflects the
# current source tree.
info "Stage 0: clear transient build caches"
rm -rf build vendor/arc42-generator/build
mkdir -p build

# ============================================================================
# Stage 1 — Pre-flight
# ============================================================================
info "Stage 1: pre-flight"

[ -f "$VERSIONS_FILE" ] || { err "$VERSIONS_FILE missing — run scripts/install-tools.sh"; exit 1; }

declare -A PINNED
while IFS=$'\t' read -r key value; do
    [ -n "${key:-}" ] || continue
    PINNED["$key"]="$value"
done < "$VERSIONS_FILE"

assert_match() {
    local key="$1" actual="$2"
    local pinned="${PINNED[$key]:-}"
    [ -n "$pinned" ] || { err "no pin recorded for $key in versions.txt"; exit 1; }
    [ "$actual" = "$pinned" ] || {
        err "$key version drift: pinned='$pinned' actual='$actual'"
        exit 1
    }
}

version_field() {
    printf '%s' "$1" | grep -oE '[0-9]+([.][0-9]+)+' | head -1
}

major_version() {
    printf '%s' "$1" | cut -d. -f1
}

assert_same_major() {
    local key="$1" actual_line="$2"
    local pinned="${PINNED[$key]:-}"
    [ -n "$pinned" ] || { err "no pin recorded for $key in versions.txt"; exit 1; }
    local pinned_major actual_major
    pinned_major="$(major_version "$(version_field "$pinned")")"
    actual_major="$(major_version "$(version_field "$actual_line")")"
    [ -n "$actual_major" ] && [ "$actual_major" = "$pinned_major" ] || {
        err "$key major version drift: pinned='$pinned' actual='$actual_line'"
        exit 1
    }
}

assert_min_major() {
    local key="$1" actual_line="$2"
    local pinned="${PINNED[$key]:-}"
    [ -n "$pinned" ] || { err "no pin recorded for $key in versions.txt"; exit 1; }
    local pinned_major actual_major
    pinned_major="$(major_version "$(version_field "$pinned")")"
    actual_major="$(major_version "$(version_field "$actual_line")")"
    [ -n "$actual_major" ] && [ "$actual_major" -ge "$pinned_major" ] || {
        err "$key major version too old: pinned='$pinned' actual='$actual_line'"
        exit 1
    }
}

assert_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        err "required command not found: $1 — run scripts/install-tools.sh"
        exit 1
    }
}

assert_cmd java
assert_cmd ruby
assert_cmd bundle
assert_cmd docker
assert_cmd git

assert_same_major "java"   "$(java --version 2>&1 | head -1)"
assert_same_major "ruby"   "$(ruby --version 2>&1)"
assert_same_major "bundler" "$(bundle --version 2>&1)"
assert_min_major  "docker" "$(docker --version 2>&1)"
docker compose version >/dev/null 2>&1 || { err "docker compose v2 not available"; exit 1; }
assert_match "docker-compose" "$(docker compose version --short 2>&1)"
assert_match "structurizr-reported" "$(java -jar tools/structurizr.war version 2>&1 \
    | grep -oE 'structurizr: [^[:space:]]+' | head -1 \
    | sed 's/^structurizr: //' | tr -d '\r')"
assert_match "cmark-gfm-reported"   "$(tools/bin/cmark-gfm --version 2>&1 | head -1)"
assert_match "pandoc-reported"      "$(tools/bin/pandoc --version 2>&1 | head -1)"

# Submodule pinned commit. `git submodule status` reads from the index,
# so it works whether or not the submodule has been committed yet (the
# leading character indicates state: ' ' clean, '-' uninitialized, '+'
# checkout differs from index, 'U' merge conflicts).
SUBMOD_STATUS=$(git submodule status vendor/arc42-generator 2>&1 | head -1)
SUBMOD_FLAG=$(printf '%s' "$SUBMOD_STATUS" | cut -c1)
case "$SUBMOD_FLAG" in
    '-') err "vendor/arc42-generator not initialized; run: git submodule update --init --recursive"; exit 1 ;;
    '+') err "vendor/arc42-generator HEAD differs from pin (status: $SUBMOD_STATUS)"; exit 1 ;;
    'U') err "vendor/arc42-generator has merge conflicts"; exit 1 ;;
esac

# Nested arc42-template submodule initialized at the SHA enforced by
# install-tools.sh from tools/arc42-template-pin.txt.
[ -f vendor/arc42-generator/arc42-template/EN/adoc/01_introduction_and_goals.adoc ] \
    || { err "vendor/arc42-generator's arc42-template at wrong revision; run scripts/install-tools.sh"; exit 1; }

# Stage 1d — transcribed-standard pin presence checks. Each pin pair
# (file + corresponding source dir) is independent. If a pin file
# exists but is empty, that's a configuration error (fail). If a pin
# file is absent, the corresponding validator will skip per the
# reconciliation gap discipline (which is enforced by the validator
# scripts themselves; Stage 1d does not block).
for pin in tools/iso-19450-opl.ebnf tools/iso-19450-opd-coherence.txt tools/iso-15288-processes.txt; do
    if [ -e "$pin" ] && [ ! -s "$pin" ]; then
        err "$pin exists but is empty — pin file must be non-empty when present"
        exit 1
    fi
done

# Stage 1e — Ruby gem dependencies installed.
bundle check >/dev/null 2>&1 || {
    err "Ruby gems not installed; run: bundle install"
    exit 1
}

# ============================================================================
# Stage 2 — Validate
# ============================================================================
info "Stage 2: validate (runs V1..V8)"
scripts/validate.sh

# ============================================================================
# Stage 3 — Render wiki-frame, 15288 lifecycle, and OPM index
# ============================================================================
# 3a — wiki-frame (Home.md, _Sidebar.md, _Footer.md). Always runs.
# (validate.sh already invokes render-wiki-frame.sh between V2 and V4 to
# give V4 inputs; calling it again here is idempotent.)
info "Stage 3a: render wiki-frame"
scripts/render-wiki-frame.sh

# 3b — ISO 15288 lifecycle pages from src/15288/*.adoc → build/15288/.
# Skipped internally when src/15288/ is absent (reconciliation gap).
info "Stage 3b: render ISO 15288 lifecycle pages"
scripts/render-15288.sh

# 3c — OPM Conceptual Model index. Skipped internally when src/opm/ absent.
info "Stage 3c: render OPM Conceptual Model index"
scripts/render-opm-index.sh

# 3d — Scientific Methods and Results pages from src/scientific/*.adoc →
# build/scientific/. Skipped internally when src/scientific/ is absent.
info "Stage 3d: render Scientific Methods and Results pages"
scripts/render-scientific.sh

# ============================================================================
# Stage 4 — Stage to wiki root
# ============================================================================
info "Stage 4: stage to wiki root"
scripts/stage-to-wiki-root.sh

# ============================================================================
# Stage 4b — Page-size guard (GitHub render limit)
# ============================================================================
# GitHub renders rich markdown only up to ~512 KiB per page; beyond that the
# live wiki silently truncates the page mid-content (this is exactly how the
# Architecture Decisions chapter lost ADR-047..060 from the rendered view).
# V1..V8 cannot catch this — they run in Stage 2, before pages are staged, and
# none checks rendered page byte size. This guard scans every staged wiki page
# and FAILS the build at the limit, with an early warning below it, so an
# oversized page surfaces here rather than silently on the published wiki.
info "Stage 4b: page-size guard (GitHub ~512 KiB render limit)"
readonly PAGE_FAIL_BYTES=524288   # 512 KiB — GitHub truncates rich rendering past this
readonly PAGE_WARN_BYTES=471040   # 460 KiB — early warning ~50 KiB before the cliff
page_oversize=0
page_warn=0
page_largest=0
page_largest_name=""
shopt -s nullglob
for pg in "$REPO_ROOT"/*.md; do
    sz=$(wc -c < "$pg")
    name=$(basename "$pg")
    if [ "$sz" -gt "$page_largest" ]; then page_largest=$sz; page_largest_name=$name; fi
    if [ "$sz" -ge "$PAGE_FAIL_BYTES" ]; then
        err "page exceeds GitHub render limit: $name is $sz bytes (limit $PAGE_FAIL_BYTES = 512 KiB) — the live wiki WILL truncate it; split the page (see stage-to-wiki-root.sh section 4e for the ADR-page precedent)"
        page_oversize=$((page_oversize + 1))
    elif [ "$sz" -ge "$PAGE_WARN_BYTES" ]; then
        printf 'build: WARNING: page %s is %s bytes (>= %s = 460 KiB), approaching the 512 KiB GitHub render limit — plan to split it before it crosses the limit\n' "$name" "$sz" "$PAGE_WARN_BYTES" >&2
        page_warn=$((page_warn + 1))
    fi
done
shopt -u nullglob
if [ "$page_oversize" -gt 0 ]; then
    err "$page_oversize page(s) exceed the GitHub render limit; build fails so the truncation does not reach the published wiki"
    exit 1
fi
info "Stage 4b: page-size guard OK ($page_warn warning(s); largest page: $page_largest_name, $page_largest bytes / 512 KiB)"

# ============================================================================
# Stage 5 — Idempotence check
# ============================================================================
info "Stage 5: idempotence check"
SCRATCH="$REPO_ROOT/build/idempotence-check"
rm -rf "$SCRATCH"
mkdir -p "$SCRATCH"
scripts/stage-to-wiki-root.sh "$SCRATCH"

diverged=0
shopt -s nullglob
for f in "$SCRATCH"/*.md "$SCRATCH"/images/*; do
    rel="${f#"$SCRATCH"/}"
    if ! cmp --quiet "$f" "$REPO_ROOT/$rel" 2>/dev/null; then
        err "non-deterministic output: $rel"
        diverged=$((diverged + 1))
    fi
done
shopt -u nullglob

if [ "$diverged" -gt 0 ]; then
    err "$diverged file(s) differ between two clean builds"
    exit 1
fi
rm -rf "$SCRATCH"

info "build complete"
