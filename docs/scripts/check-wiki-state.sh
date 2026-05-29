#!/usr/bin/env bash
#
# check-wiki-state.sh
#
# Post-push detection script. Clones the live wiki repo into a scratch
# directory, re-runs build.sh Stage 4 ONLY against its committed sources,
# then compares each staged file against the live wiki root via cmp(1).
#
# Maintainers run this manually (or from a periodic cron job in their own
# environment) to detect commits where source and rendered output drifted.
# Repair: clone, run scripts/build.sh, commit the regenerated output, push.

set -euo pipefail

readonly REMOTE="https://github.com/UOR-Foundation/UOR-Framework.wiki.git"
readonly SCRATCH=$(mktemp -d -t wiki-check.XXXXXX)
trap 'rm -rf "$SCRATCH"' EXIT

err()  { printf 'check-wiki-state: ERROR: %s\n' "$*" >&2; }
info() { printf 'check-wiki-state: %s\n' "$*"; }

info "cloning $REMOTE -> $SCRATCH"
git clone --quiet --recurse-submodules "$REMOTE" "$SCRATCH"

info "re-running Stage 3 (a/b/c) + Stage 4 against committed sources"
( cd "$SCRATCH" && bash scripts/render-wiki-frame.sh )
( cd "$SCRATCH" && bash scripts/render-15288.sh )
( cd "$SCRATCH" && bash scripts/render-opm-index.sh )
( cd "$SCRATCH" && bash scripts/stage-to-wiki-root.sh "$SCRATCH/.staged" )

# Note: Stage 4 above wrote outputs to $SCRATCH/.staged/. The reason we
# don't write back to $SCRATCH itself: doing so would overwrite the
# committed files we want to compare against.

diverged=0
mapfile -t files < <(
    cd "$SCRATCH/.staged" && find . -maxdepth 2 -type f -printf '%P\n'
)

[ "${#files[@]}" -gt 0 ] || { err "no staged files produced; aborting"; exit 2; }

for rel in "${files[@]}"; do
    if ! cmp --quiet "$SCRATCH/.staged/$rel" "$SCRATCH/$rel" 2>/dev/null; then
        err "DRIFT: $rel"
        diverged=$((diverged + 1))
    fi
done

if [ "$diverged" -gt 0 ]; then
    err "$diverged file(s) drifted between source and committed wiki root"
    err "repair: clone, run scripts/build.sh, commit regenerated output, push"
    exit 1
fi

info "PASS — wiki root is byte-identical to a re-staged build"
