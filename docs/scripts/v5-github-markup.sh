#!/usr/bin/env bash
#
# v5-github-markup.sh
#
# Shell wrapper around v5-github-markup.rb. Invokes the Ruby script under
# `bundle exec` so the pinned github-markup gem version is used.
#
# Targets passed as positional args; defaults to all .md files at the
# repo root if none given.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly RUBY_SCRIPT="$REPO_ROOT/scripts/v5-github-markup.rb"

err() { printf 'V5: ERROR: %s\n' "$*" >&2; }

[ -f "$RUBY_SCRIPT" ] || { err "$RUBY_SCRIPT missing"; exit 2; }

if [ "$#" -eq 0 ]; then
    mapfile -t TARGETS < <(find "$REPO_ROOT" -maxdepth 1 -name '*.md' -type f | sort)
else
    TARGETS=("$@")
fi

[ "${#TARGETS[@]}" -gt 0 ] || { printf 'V5: no .md files to validate\n'; exit 0; }

cd "$REPO_ROOT"
exec bundle exec ruby "$RUBY_SCRIPT" "${TARGETS[@]}"
