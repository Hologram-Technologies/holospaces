#!/usr/bin/env bash
#
# stage-to-wiki-root.sh
#
# build.sh Stage 4 implementation.
#
# Inputs (already produced by earlier stages):
#   vendor/arc42-generator/build/EN/gitHubMarkdownMP/plain/NN_chapter_name.md
#   build/wiki-frame/{Home.md,_Sidebar.md,_Footer.md}
#   src/arc42/images/*.svg                               (from V3)
#
# Outputs at the wiki repo's root:
#   {01..12}-Title-Case-Name.md                          (12 chapters)
#   Home.md, _Sidebar.md, _Footer.md                     (wiki frame)
#   images/                                              (committed SVGs)
#
# Optional argument: a destination root other than the repo root.
# Used by check-wiki-state.sh and build.sh Stage 5 to stage into a scratch
# directory for comparison against the actual wiki root.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly DEST="${1:-$REPO_ROOT}"

readonly GEN_OUT="$REPO_ROOT/vendor/arc42-generator/build/EN/gitHubMarkdownMP/plain"
readonly FRAME_OUT="$REPO_ROOT/build/wiki-frame"
readonly SRC_IMAGES="$REPO_ROOT/src/arc42/images"
readonly LIFECYCLE_OUT="$REPO_ROOT/build/15288"
readonly OPM_OUT="$REPO_ROOT/build/opm"
readonly OPM_SRC="$REPO_ROOT/src/opm"
readonly SCIENTIFIC_OUT="$REPO_ROOT/build/scientific"

err()  { printf 'stage: ERROR: %s\n' "$*" >&2; }
info() { printf 'stage: %s\n' "$*"; }

mkdir -p "$DEST/images"

# ---- 4a. Rename and copy the 12 chapter markdown files ------------------
#
# Transform: NN_chapter_name.md -> NN-Chapter-Name.md
#   - Leading "NN_" -> "NN-"
#   - Remaining "_" -> "-"
#   - Title-case each word; "and" stays lower-case unless it's the first word

title_case_filename() {
    local base="$1"                                 # e.g. 03_context_and_scope
    local num="${base%%_*}"                         # 03
    local rest="${base#*_}"                         # context_and_scope
    local out="$num"
    local IFS=_
    local first=1
    for word in $rest; do
        local cased
        if [ "$word" = "and" ] && [ "$first" -eq 0 ]; then
            cased="and"
        else
            cased="$(printf '%s' "${word:0:1}" | tr '[:lower:]' '[:upper:]')${word:1}"
        fi
        out+="-$cased"
        first=0
    done
    printf '%s' "$out"
}

[ -d "$GEN_OUT" ] || { err "$GEN_OUT missing (V2 must run first)"; exit 2; }

count=0
for src in "$GEN_OUT"/*.md; do
    base=$(basename "$src" .md)
    case "$base" in
        arc42-template-EN) continue ;;              # umbrella file; not staged
        [0-9][0-9]_*) ;;                            # chapter file
        *) continue ;;                              # skip anything else
    esac
    out_name="$(title_case_filename "$base").md"
    cp "$src" "$DEST/$out_name"
    count=$((count + 1))
done
info "staged $count chapter file(s)"

# ---- 4b. Wiki-frame files ----------------------------------------------

[ -d "$FRAME_OUT" ] || { err "$FRAME_OUT missing (render-wiki-frame.sh must run first)"; exit 2; }
for f in Home.md _Sidebar.md _Footer.md; do
    [ -f "$FRAME_OUT/$f" ] || { err "$FRAME_OUT/$f missing"; exit 2; }
    cp "$FRAME_OUT/$f" "$DEST/$f"
done
info "staged wiki-frame files"

# ---- 4c. SVGs ----------------------------------------------------------
# Combines V3-exported C4 SVGs and per-OPD opd.svg files. The opm- prefix
# on OPD SVGs prevents name collisions with V3 output.
#
# Wipe c4-* and opm-* SVGs from the destination first so a removed view
# (or a removed -key.svg companion) does not leave a stale file behind.
# Other SVGs at $DEST/images/ (hand-authored) are preserved.

find "$DEST/images" -maxdepth 1 -name 'c4-*.svg' -delete 2>/dev/null || true
find "$DEST/images" -maxdepth 1 -name 'opm-*.svg' -delete 2>/dev/null || true

svg_count=0
if [ -d "$SRC_IMAGES" ] && [ -n "$(ls -A "$SRC_IMAGES" 2>/dev/null | grep -E '\.svg$' || true)" ]; then
    cp "$SRC_IMAGES"/*.svg "$DEST/images/"
    svg_count=$((svg_count + $(ls "$SRC_IMAGES"/*.svg 2>/dev/null | wc -l)))
fi

if [ -d "$OPM_SRC" ]; then
    for opd_dir in "$OPM_SRC"/*/; do
        [ -d "$opd_dir" ] || continue
        local_opd_name=$(basename "$opd_dir")
        if [ -f "$opd_dir/opd.svg" ]; then
            cp "$opd_dir/opd.svg" "$DEST/images/opm-$local_opd_name.svg"
            svg_count=$((svg_count + 1))
        fi
    done
fi

if [ "$svg_count" -gt 0 ]; then
    rm -f "$DEST/images/.gitkeep"
    info "staged $svg_count SVG(s)"
else
    : > "$DEST/images/.gitkeep"
    info "no SVGs to stage; placed images/.gitkeep"
fi

# ---- 4d. Rewrite image references --------------------------------------
# arc42-generator emits image references via the upstream EN/images/ path
# layout. The wiki uses a flat images/ directory at the repo root. Rewrite
# the reference prefix in each staged chapter markdown.

for md in "$DEST"/[0-9][0-9]-*.md; do
    [ -f "$md" ] || continue
    # Match Markdown image syntax: ![alt](path/to/file.svg)
    # Replace any leading path segment with "images/" while preserving the basename.
    sed -E -i 's|!\[([^]]*)\]\(([^)]*/)?([^/)]+\.svg)\)|![\1](images/\3)|g' "$md"
done
info "rewrote image references"

# ---- 4e. Split the Architecture Decisions page (GitHub 512 KiB limit) ---
# GitHub renders rich markdown only up to ~512 KiB per page; beyond that the
# page is truncated mid-content. The Architecture Decisions chapter exceeds
# this (60+ ADRs), so split the staged page into two at the ADR-038 boundary —
# the start of the "substrate-amendment audit trail (ADR-038 through ADR-NNN)"
# cohort. Page 1 keeps the chapter intro, the audit-trail table, and ADR-001
# through ADR-037; page 2 (09-Architecture-Decisions-Continued.md) carries
# ADR-038 onward. New ADRs append to page 2; page 1 (ADR-001..037) is stable,
# so the split point and page-1 size do not drift. Deterministic: the split is
# the first line matching the ADR-038 section heading, so Stage 5 idempotence
# holds. The split runs after image-reference rewriting so page 2 inherits the
# already-rewritten page-1 tail.

ad_page1="$DEST/09-Architecture-Decisions.md"
ad_page2="$DEST/09-Architecture-Decisions-Continued.md"
# Only split when the page would exceed GitHub's ~512 KiB render limit. A small
# Architecture Decisions chapter (e.g. holospaces' handful of ADRs) is left
# whole; the ADR-038 split point is a wiki convention for its 60+-ADR corpus.
ad_split_threshold=524288
if [ -f "$ad_page1" ] && [ "$(wc -c < "$ad_page1")" -ge "$ad_split_threshold" ]; then
    split_line=$(grep -nE '^## ADR-038 ' "$ad_page1" | head -1 | cut -d: -f1)
    if [ -n "$split_line" ]; then
        {
            printf '# Architecture Decisions (continued)\n\n'
            printf '**← [Architecture Decisions: ADR-001 through ADR-037](09-Architecture-Decisions)**\n\n'
            printf 'This page continues [9. Architecture Decisions](09-Architecture-Decisions), carrying ADR-038 onward. The chapter is split across two pages because GitHub renders rich markdown only up to ~512 KiB per page; the audit-trail table and ADR-001 through ADR-037 are on the first page.\n\n'
            printf -- '---\n\n'
            tail -n +"$split_line" "$ad_page1"
        } > "$ad_page2"
        {
            head -n "$((split_line - 1))" "$ad_page1"
            printf -- '---\n\n'
            printf '**Architecture Decisions continues → [ADR-038 onward](09-Architecture-Decisions-Continued)** (the chapter is split across two pages to stay within GitHub'\''s ~512 KiB per-page rendering limit).\n'
        } > "$ad_page1.tmp"
        mv "$ad_page1.tmp" "$ad_page1"
        info "split Architecture Decisions at ADR-038 → 09-Architecture-Decisions-Continued.md"
    else
        err "Architecture Decisions page exceeds the GitHub render limit but has no '## ADR-038 ' split boundary"
        exit 2
    fi
fi

# ---- 4e. ISO 15288 lifecycle pages ------------------------------------
# Map src/15288/<group>.adoc → Lifecycle-<TitleCase-Group>-Processes.md.
# Mapping is a fixed table (no heuristic). Skipped when build/15288/
# is absent (reconciliation gap).

declare -A LIFECYCLE_MAP=(
    [agreement]="Lifecycle-Agreement-Processes.md"
    [organizational-project-enabling]="Lifecycle-Organizational-Project-Enabling-Processes.md"
    [technical-management]="Lifecycle-Technical-Management-Processes.md"
    [technical]="Lifecycle-Technical-Processes.md"
)

if [ -d "$LIFECYCLE_OUT" ]; then
    lifecycle_count=0
    for src_basename in "${!LIFECYCLE_MAP[@]}"; do
        src_md="$LIFECYCLE_OUT/$src_basename.md"
        if [ -f "$src_md" ]; then
            cp "$src_md" "$DEST/${LIFECYCLE_MAP[$src_basename]}"
            lifecycle_count=$((lifecycle_count + 1))
        fi
    done
    [ "$lifecycle_count" -gt 0 ] && info "staged $lifecycle_count lifecycle page(s)"
fi

# ---- 4f. OPM Conceptual Model index page -------------------------------
# Skipped when build/opm/Conceptual-Model.md is absent.

if [ -f "$OPM_OUT/Conceptual-Model.md" ]; then
    cp "$OPM_OUT/Conceptual-Model.md" "$DEST/Conceptual-Model.md"
    info "staged Conceptual-Model.md"
fi

# ---- 4g. Scientific Methods and Results pages --------------------------
# Each src/scientific/<page>.adoc renders to build/scientific/<page>.md and
# stages to $DEST/<page>.md unchanged (the source filename is already the
# wiki-URL slug, so no rename is applied — unlike the 15288 lifecycle pages
# whose source filenames are short group keys).
# Skipped when build/scientific/ is absent (reconciliation gap).

if [ -d "$SCIENTIFIC_OUT" ]; then
    scientific_count=0
    for src_md in "$SCIENTIFIC_OUT"/*.md; do
        [ -f "$src_md" ] || continue
        cp "$src_md" "$DEST/$(basename "$src_md")"
        scientific_count=$((scientific_count + 1))
    done
    [ "$scientific_count" -gt 0 ] && info "staged $scientific_count scientific page(s)"
fi
