#!/usr/bin/env bash
#
# render-wiki-frame.sh
#
# Renders src/wiki/{Home,Sidebar,Footer}.adoc to GFM markdown via pandoc.
# Outputs land at build/wiki-frame/{Home.md,_Sidebar.md,_Footer.md}.
# The leading underscore on Sidebar/Footer is the GitHub wiki convention.
#
# Shared by validate.sh (so V4 has wiki-frame inputs) and build.sh Stage 3.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly SRC_DIR="$REPO_ROOT/src/wiki"
readonly OUT_DIR="$REPO_ROOT/build/wiki-frame"
readonly PANDOC_BIN="$REPO_ROOT/tools/bin/pandoc"

err()  { printf 'render-wiki-frame: ERROR: %s\n' "$*" >&2; }
info() { printf 'render-wiki-frame: %s\n' "$*"; }

# Use the pinned pandoc, not whatever happens to be on the host PATH —
# pandoc minor-version differences can shift the docbook→GFM output.
[ -x "$PANDOC_BIN" ] || { err "pinned pandoc missing at $PANDOC_BIN (run scripts/install-tools.sh)"; exit 2; }
[ -d "$SRC_DIR" ] || { err "$SRC_DIR missing"; exit 2; }

mkdir -p "$OUT_DIR"

# Pandoc has no native AsciiDoc reader, so the canonical conversion path is
# asciidoctor → DocBook → pandoc → GFM. asciidoctor is a Bundler dependency
# (also used by V1), so we invoke it under bundle exec for version pinning.

render() {
    local src="$1"
    local out="$2"
    local tmp_xml
    tmp_xml=$(mktemp -t wiki-frame.XXXXXX.xml)
    ( cd "$REPO_ROOT" && bundle exec asciidoctor -b docbook5 -o "$tmp_xml" "$src" )
    "$PANDOC_BIN" -f docbook -t gfm --wrap=none "$tmp_xml" -o "$out"
    rm -f "$tmp_xml"
    info "rendered $(basename "$src") -> $(basename "$out")"
}

render "$SRC_DIR/Home.adoc"    "$OUT_DIR/Home.md"
render "$SRC_DIR/Sidebar.adoc" "$OUT_DIR/_Sidebar.md"
render "$SRC_DIR/Footer.adoc"  "$OUT_DIR/_Footer.md"
