#!/usr/bin/env bash
#
# v7-opd-opl-coherence.sh
#
# For each src/opm/<opd-name>/ subdirectory, asserts that the entity
# name set declared in opl.txt (per the productions named in
# tools/iso-19450-opd-coherence.txt) equals the set of <text> element
# content strings in opd.svg. Equality is exact (whitespace-trimmed);
# no fuzzy matching.
#
# Reconciliation gap discipline:
#   - If tools/iso-19450-opd-coherence.txt is absent, skip with reason.
#   - If tools/iso-19450-opl.ebnf is absent, skip (V7 reuses V6's parser).
#   - If src/opm/ is absent or empty, skip with reason.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly COHERENCE_FILE="$REPO_ROOT/tools/iso-19450-opd-coherence.txt"
readonly EBNF_FILE="$REPO_ROOT/tools/iso-19450-opl.ebnf"
readonly OPM_DIR="$REPO_ROOT/src/opm"
readonly RB_SCRIPT="$REPO_ROOT/scripts/v7-opd-opl-coherence.rb"

if [ ! -f "$COHERENCE_FILE" ]; then
    printf 'V7: SKIP (pin file absent: %s)\n' "tools/iso-19450-opd-coherence.txt"
    exit 0
fi
if [ ! -f "$EBNF_FILE" ]; then
    printf 'V7: SKIP (V6 pin file absent — V7 reuses V6 parser: %s)\n' "tools/iso-19450-opl.ebnf"
    exit 0
fi
if [ ! -d "$OPM_DIR" ] || ! find "$OPM_DIR" -maxdepth 2 -name 'opl.txt' -print -quit | grep -q .; then
    printf 'V7: SKIP (source directory absent or empty — no inputs to validate: %s)\n' "src/opm/"
    exit 0
fi

cd "$REPO_ROOT"
exec bundle exec ruby "$RB_SCRIPT" "$EBNF_FILE" "$COHERENCE_FILE" "$OPM_DIR"
