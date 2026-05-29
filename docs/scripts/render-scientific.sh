#!/usr/bin/env bash
#
# render-scientific.sh
#
# build.sh Stage 3d implementation: renders src/scientific/*.adoc to
# build/scientific/*.md via the asciidoctor → DocBook → pandoc → GFM
# pipeline (same conversion path as the wiki-frame and 15288 renderers).
#
# This stage produces the academic-documentation partition pages
# ("Scientific Methods and Results" and any future siblings authored
# by the academic working group, per ADR-031's normative-vs-academic
# content distinction). Robin Wikoff submits content via GitHub issues;
# the maintainer authors AsciiDoc under src/scientific/, runs build.sh,
# and pushes the regenerated wiki root.
#
# Reconciliation gap discipline: skip cleanly when src/scientific/ is
# absent or has no .adoc files.
#
# Shared by build.sh Stage 3d, validate.sh's render_scientific, and
# check-wiki-state.sh.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly SRC_DIR="$REPO_ROOT/src/scientific"
readonly OUT_DIR="$REPO_ROOT/build/scientific"
readonly PANDOC_BIN="$REPO_ROOT/tools/bin/pandoc"

err()  { printf 'render-scientific: ERROR: %s\n' "$*" >&2; }
info() { printf 'render-scientific: %s\n' "$*"; }

if [ ! -d "$SRC_DIR" ] || ! find "$SRC_DIR" -maxdepth 1 -name '*.adoc' -print -quit | grep -q .; then
    info "src/scientific/ absent or has no .adoc files — skipping (reconciliation gap)"
    exit 0
fi

[ -x "$PANDOC_BIN" ] || { err "pinned pandoc missing at $PANDOC_BIN (run scripts/install-tools.sh)"; exit 2; }

mkdir -p "$OUT_DIR"

count=0
for adoc in "$SRC_DIR"/*.adoc; do
    [ -f "$adoc" ] || continue
    base=$(basename "$adoc" .adoc)
    tmp_xml=$(mktemp -t scientific.XXXXXX.xml)
    ( cd "$REPO_ROOT" && bundle exec asciidoctor -b docbook5 -o "$tmp_xml" "$adoc" )
    "$PANDOC_BIN" -f docbook -t gfm --wrap=none "$tmp_xml" -o "$OUT_DIR/$base.md"
    rm -f "$tmp_xml"
    count=$((count + 1))
    info "rendered $base.adoc -> $base.md"
done

info "rendered $count scientific page(s)"
