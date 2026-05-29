#!/usr/bin/env bash
#
# validate.sh — top-level entry point: runs V1..V8 and aggregates a single
# report. V6/V7/V8 follow reconciliation-gap discipline (skip when their
# pin file or source dir is absent — pass-by-vacuity).
#
# Order: V1, V3, V6, V8 (independent). V7 (after V6). V2 (after V3). V4
# (after V2 + render_frame + render_15288 + render_opm). V5 (after V4).

set -uo pipefail        # NOT -e: keep going across validators

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$REPO_ROOT"

# Force a clean validation: clear transient build caches so V2 / V4 / V5
# can never validate against stale outputs from a prior run. The
# arc42-generator's gradle/docker pipeline (V2) caches per-chapter MD
# outputs in vendor/arc42-generator/build/, and historically retained
# stale outputs when source AsciiDoc changed in ways the incremental-build
# cache didn't recognize. Clearing both build/ (stage outputs consumed by
# V4/V5) and vendor/arc42-generator/build/ (V2's gradle outputs) at the
# start of every validation run is the architectural guarantee that the
# validators always operate on the current source tree.
rm -rf build vendor/arc42-generator/build
mkdir -p build

# Status values: pass, fail, skip-gap, skip-upstream
V1=skip; V2=skip; V3=skip; V4=skip; V5=skip
V6=skip; V7=skip; V8=skip
V1_REASON=""; V2_REASON=""; V3_REASON=""; V4_REASON=""; V5_REASON=""
V6_REASON=""; V7_REASON=""; V8_REASON=""

run_v1() {
    if bundle exec ruby scripts/v1-structural-alignment.rb 2>>build/validate.err; then
        V1=pass
    else
        V1=fail; V1_REASON="see build/validate.err"
    fi
}
run_v3() {
    if scripts/v3-structurizr.sh 2>>build/validate.err; then
        V3=pass
    else
        V3=fail; V3_REASON="see build/validate.err"
    fi
}
run_v2() {
    if scripts/v2-arc42-build.sh 2>>build/validate.err; then
        V2=pass
    else
        V2=fail; V2_REASON="see build/validate.err"
    fi
}
render_frame() {
    scripts/render-wiki-frame.sh 2>>build/validate.err
}
render_15288() {
    # Stage 3b — delegate to the shared render-15288.sh helper.
    # Skips internally when src/15288/ is absent.
    scripts/render-15288.sh 2>>build/validate.err
}
render_opm() {
    # Stage 3c — assemble build/opm/Conceptual-Model.md. Skips internally
    # when src/opm/ is absent.
    scripts/render-opm-index.sh 2>>build/validate.err
}
render_scientific() {
    # Stage 3d — render src/scientific/*.adoc → build/scientific/*.md.
    # Skips internally when src/scientific/ is absent.
    scripts/render-scientific.sh 2>>build/validate.err
}
collect_md_targets() {
    # Echo the list of .md files V4/V5 should validate. Includes:
    # - V2 chapter outputs (vendor/arc42-generator/build/EN/gitHubMarkdownMP/plain)
    # - wiki-frame outputs (build/wiki-frame)
    # - 15288 outputs (build/15288)
    # - OPM index (build/opm)
    if [ -d "vendor/arc42-generator/build/EN/gitHubMarkdownMP/plain" ]; then
        find vendor/arc42-generator/build/EN/gitHubMarkdownMP/plain -maxdepth 1 -name '*.md' -type f
    fi
    if [ -d "build/wiki-frame" ]; then
        find build/wiki-frame -maxdepth 1 -name '*.md' -type f
    fi
    if [ -d "build/15288" ]; then
        find build/15288 -maxdepth 1 -name '*.md' -type f
    fi
    if [ -d "build/opm" ]; then
        find build/opm -maxdepth 1 -name '*.md' -type f
    fi
    if [ -d "build/scientific" ]; then
        find build/scientific -maxdepth 1 -name '*.md' -type f
    fi
}
run_v4() {
    local targets=()
    while IFS= read -r f; do targets+=("$f"); done < <(collect_md_targets)
    if [ "${#targets[@]}" -eq 0 ]; then
        V4=skip; V4_REASON="no markdown to validate"; return
    fi
    if scripts/v4-cmark-gfm.sh "${targets[@]}" 2>>build/validate.err; then
        V4=pass
    else
        V4=fail; V4_REASON="see build/validate.err"
    fi
}
run_v5() {
    local targets=()
    while IFS= read -r f; do targets+=("$f"); done < <(collect_md_targets)
    if [ "${#targets[@]}" -eq 0 ]; then
        V5=skip; V5_REASON="no markdown to validate"; return
    fi
    if scripts/v5-github-markup.sh "${targets[@]}" 2>>build/validate.err; then
        V5=pass
    else
        V5=fail; V5_REASON="see build/validate.err"
    fi
}

# V6/V7/V8 wrappers absorb their own gap-skip discipline. The shell
# script writes a "SKIP" line and exits 0 when its pin file or source
# is absent; the script writes its own "PASS" line and exits 0 on
# success; non-zero on failure. We capture stdout to distinguish.
run_v6() {
    local out
    out=$(scripts/v6-opl-syntax.sh 2>>build/validate.err)
    local rc=$?
    if [ $rc -ne 0 ]; then
        V6=fail; V6_REASON="see build/validate.err"
    elif printf '%s' "$out" | grep -q '^V6: SKIP'; then
        V6=skip-gap; V6_REASON=$(printf '%s' "$out" | sed -n 's/^V6: SKIP (\(.*\))$/\1/p' | head -1)
    else
        V6=pass
    fi
}
run_v7() {
    if [ "$V6" = "fail" ]; then
        V7=skip-upstream; V7_REASON="V6 failed"; return
    fi
    local out
    out=$(scripts/v7-opd-opl-coherence.sh 2>>build/validate.err)
    local rc=$?
    if [ $rc -ne 0 ]; then
        V7=fail; V7_REASON="see build/validate.err"
    elif printf '%s' "$out" | grep -q '^V7: SKIP'; then
        V7=skip-gap; V7_REASON=$(printf '%s' "$out" | sed -n 's/^V7: SKIP (\(.*\))$/\1/p' | head -1)
    else
        V7=pass
    fi
}
run_v8() {
    local out
    out=$(scripts/v8-iso-15288-superset.sh 2>>build/validate.err)
    local rc=$?
    if [ $rc -ne 0 ]; then
        V8=fail; V8_REASON="see build/validate.err"
    elif printf '%s' "$out" | grep -q '^V8: SKIP'; then
        V8=skip-gap; V8_REASON=$(printf '%s' "$out" | sed -n 's/^V8: SKIP (\(.*\))$/\1/p' | head -1)
    else
        V8=pass
    fi
}

: > build/validate.err

# Independent validators
run_v1
run_v3
run_v6
run_v8
# V7 chains after V6
run_v7
# Architecture chain: V2 → render → V4 → V5
if [ "$V3" = "pass" ]; then
    run_v2
else
    V2=skip-upstream; V2_REASON="V3 failed"
fi
if [ "$V2" = "pass" ]; then
    render_frame
    render_15288
    render_opm
    render_scientific
fi
if [ "$V2" = "pass" ]; then
    run_v4
else
    V4=skip-upstream; V4_REASON="V2 failed or skipped"
fi
if [ "$V4" = "pass" ]; then
    run_v5
else
    V5=skip-upstream; V5_REASON="V4 failed or skipped"
fi

emoji() {
    case "$1" in
        pass) printf '[x]' ;;
        fail) printf '[ ]' ;;
        skip-gap) printf '[~]' ;;
        skip-upstream) printf '[-]' ;;
        skip) printf '[-]' ;;
    esac
}
report_line() {
    local mark name status reason
    mark=$(emoji "$2")
    name="$1"
    status="$2"
    reason="$3"
    if [ -n "$reason" ]; then
        printf '%s %s (%s — %s)\n' "$mark" "$name" "$status" "$reason"
    else
        printf '%s %s (%s)\n' "$mark" "$name" "$status"
    fi
}

printf '\nValidation report:\n'
report_line "V1 — arc42 structural alignment"      "$V1" "$V1_REASON"
report_line "V3 — Structurizr DSL"                 "$V3" "$V3_REASON"
report_line "V2 — arc42 build pipeline"            "$V2" "$V2_REASON"
report_line "V4 — CommonMark / GFM"                "$V4" "$V4_REASON"
report_line "V5 — GitHub-markup rendering"         "$V5" "$V5_REASON"
report_line "V6 — OPL syntax"                      "$V6" "$V6_REASON"
report_line "V7 — OPD/OPL bimodal coherence"       "$V7" "$V7_REASON"
report_line "V8 — ISO 15288 process superset"      "$V8" "$V8_REASON"
printf '\n'

# Exit 0 iff every validator is either `pass` or `skip-gap`. A
# `skip-upstream` (caused by an earlier validator's failure) does NOT
# permit exit 0 — the upstream failure itself is what fails the run.
ok=1
for s in "$V1" "$V2" "$V3" "$V4" "$V5" "$V6" "$V7" "$V8"; do
    case "$s" in
        pass|skip-gap) ;;
        *) ok=0 ;;
    esac
done
[ "$ok" -eq 1 ] && exit 0
exit 1
