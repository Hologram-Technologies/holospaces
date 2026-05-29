#!/usr/bin/env bash
#
# render-opm-index.sh
#
# build.sh Stage 3c implementation: assembles build/opm/Conceptual-Model.md
# by iterating src/opm/opd-order.txt and emitting, for each named OPD:
#   - a heading naming the OPD (subdirectory name, hyphens → spaces, title-cased)
#   - a markdown image reference to images/opm-<opd-name>.svg
#   - the OPL prose block, wrapped in a ```opl GFM fenced code block
#
# Reconciliation gap discipline: skip cleanly when src/opm/ is absent
# or empty. The output file build/opm/Conceptual-Model.md is created
# only when at least one OPD exists.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly OPM_DIR="$REPO_ROOT/src/opm"
readonly ORDER_FILE="$OPM_DIR/opd-order.txt"
readonly OUT_DIR="$REPO_ROOT/build/opm"
readonly OUT_FILE="$OUT_DIR/Conceptual-Model.md"

err()  { printf 'render-opm-index: ERROR: %s\n' "$*" >&2; }
info() { printf 'render-opm-index: %s\n' "$*"; }

if [ ! -d "$OPM_DIR" ] || [ ! -f "$ORDER_FILE" ]; then
    info "src/opm/ absent or has no opd-order.txt — skipping (reconciliation gap)"
    exit 0
fi

mkdir -p "$OUT_DIR"
: > "$OUT_FILE"

# Print top-of-page heading + optional narrative intro
{
    printf '# Conceptual Model\n\n'
    if [ -f "$OPM_DIR/intro.md" ]; then
        cat "$OPM_DIR/intro.md"
        printf '\n'
    else
        printf 'OPM (ISO 19450) conceptual model of Prism. Each OPD is presented '
        printf 'with its rendering and the canonical OPL declaration.\n\n'
    fi
} >> "$OUT_FILE"

# Convert kebab-case identifier into a Title Case heading.
title_case() {
    local s="$1"
    s="${s//-/ }"
    awk '{
        for (i=1; i<=NF; i++) {
            $i = toupper(substr($i, 1, 1)) substr($i, 2)
        }
        print
    }' <<<"$s"
}

emitted=0
while IFS= read -r opd_name; do
    opd_name="${opd_name#"${opd_name%%[![:space:]]*}"}"
    opd_name="${opd_name%"${opd_name##*[![:space:]]}"}"
    [ -z "$opd_name" ] && continue
    [ "${opd_name:0:1}" = "#" ] && continue

    opd_path="$OPM_DIR/$opd_name"
    opl_file="$opd_path/opl.txt"
    svg_file="$opd_path/opd.svg"

    if [ ! -d "$opd_path" ]; then
        err "OPD '$opd_name' listed in opd-order.txt but $opd_path does not exist"
        exit 1
    fi
    if [ ! -f "$opl_file" ]; then
        err "OPD '$opd_name' missing opl.txt"
        exit 1
    fi
    if [ ! -f "$svg_file" ]; then
        err "OPD '$opd_name' missing opd.svg"
        exit 1
    fi

    title=$(title_case "$opd_name")
    narrative_file="$opd_path/narrative.md"
    {
        printf '## %s\n\n' "$title"
        if [ -f "$narrative_file" ]; then
            cat "$narrative_file"
            printf '\n'
        fi
        printf '![%s](images/opm-%s.svg)\n\n' "$title" "$opd_name"
        printf '```opl\n'
        # opl.txt already ends with a newline; emit it verbatim and
        # close the fence without an extra blank line.
        cat "$opl_file"
        printf '```\n\n'
    } >> "$OUT_FILE"
    emitted=$((emitted + 1))
done < "$ORDER_FILE"

if [ "$emitted" -eq 0 ]; then
    info "opd-order.txt empty — Conceptual-Model.md remains skeleton-only"
else
    info "rendered Conceptual-Model.md ($emitted OPD(s))"
fi
