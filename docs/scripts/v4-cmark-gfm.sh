#!/usr/bin/env bash
#
# v4-cmark-gfm.sh
#
# Runs the pinned cmark-gfm against every .md file at the wiki root, with
# the GFM extension set GitHub itself enables for wiki rendering. Catches
# non-UTF-8 bytes and parser errors. Permissive markdown defects (broken
# links, unbalanced emphasis the parser recovers from) are out of scope.
#
# Targets passed as positional args; defaults to all .md files at the
# repo root if none given.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly CMARK="$REPO_ROOT/tools/bin/cmark-gfm"

err()  { printf 'V4: ERROR: %s\n' "$*" >&2; }
info() { printf 'V4: %s\n' "$*"; }

[ -x "$CMARK" ] || { err "$CMARK missing (run scripts/install-tools.sh)"; exit 2; }

if [ "$#" -eq 0 ]; then
    mapfile -t TARGETS < <(find "$REPO_ROOT" -maxdepth 1 -name '*.md' -type f | sort)
else
    TARGETS=("$@")
fi

[ "${#TARGETS[@]}" -gt 0 ] || { info "no .md files to validate"; exit 0; }

failed=0
for f in "${TARGETS[@]}"; do
    if ! "$CMARK" \
            --validate-utf8 \
            --extension table \
            --extension strikethrough \
            --extension autolink \
            --extension tagfilter \
            --extension tasklist \
            "$f" >/dev/null 2>"$REPO_ROOT/build/v4-${f##*/}.err"; then
        err "$f"
        cat "$REPO_ROOT/build/v4-${f##*/}.err" >&2
        failed=$((failed + 1))
    else
        rm -f "$REPO_ROOT/build/v4-${f##*/}.err"
    fi
done

if [ "$failed" -gt 0 ]; then
    err "$failed file(s) failed"
    exit 1
fi
info "PASS (${#TARGETS[@]} file(s))"
