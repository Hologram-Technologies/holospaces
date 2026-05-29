#!/usr/bin/env bash
#
# render-15288.sh
#
# build.sh Stage 3b implementation: renders src/15288/*.adoc to
# build/15288/*.md via the asciidoctor → DocBook → pandoc → GFM
# pipeline (same conversion path as the wiki-frame renderer).
#
# Reconciliation gap discipline: skip cleanly when src/15288/ is
# absent or has no .adoc files.
#
# Shared by build.sh Stage 3b, validate.sh's render_15288, and
# check-wiki-state.sh.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly SRC_DIR="$REPO_ROOT/src/15288"
readonly OUT_DIR="$REPO_ROOT/build/15288"
readonly PANDOC_BIN="$REPO_ROOT/tools/bin/pandoc"

err()  { printf 'render-15288: ERROR: %s\n' "$*" >&2; }
info() { printf 'render-15288: %s\n' "$*"; }

if [ ! -d "$SRC_DIR" ] || ! find "$SRC_DIR" -maxdepth 1 -name '*.adoc' -print -quit | grep -q .; then
    info "src/15288/ absent or has no .adoc files — skipping (reconciliation gap)"
    exit 0
fi

[ -x "$PANDOC_BIN" ] || { err "pinned pandoc missing at $PANDOC_BIN (run scripts/install-tools.sh)"; exit 2; }

mkdir -p "$OUT_DIR"

count=0
for adoc in "$SRC_DIR"/*.adoc; do
    [ -f "$adoc" ] || continue
    base=$(basename "$adoc" .adoc)
    tmp_xml=$(mktemp -t lifecycle.XXXXXX.xml)
    ( cd "$REPO_ROOT" && bundle exec asciidoctor -b docbook5 -o "$tmp_xml" "$adoc" )
    "$PANDOC_BIN" -f docbook -t gfm --wrap=none "$tmp_xml" -o "$OUT_DIR/$base.md"
    rm -f "$tmp_xml"
    count=$((count + 1))
    info "rendered $base.adoc -> $base.md"
done

info "rendered $count lifecycle page(s)"
