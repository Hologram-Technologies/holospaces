#!/usr/bin/env bash
#
# v6-opl-syntax.sh
#
# Parses each src/opm/<opd-name>/opl.txt file against the pinned ISO 19450
# Annex A OPL EBNF grammar at tools/iso-19450-opl.ebnf via the treetop
# Ruby PEG parser.
#
# Reconciliation gap discipline (per wiki-definition.md):
#   - If tools/iso-19450-opl.ebnf is absent, exit 0 with a "skipped" line
#     to stdout naming the gap reason. This is not failure.
#   - If src/opm/ is absent or contains no opl.txt files, exit 0 with a
#     skipped line. Pass-by-vacuity.
#   - Otherwise invoke v6-opl-syntax.rb for the actual parse check.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly EBNF_FILE="$REPO_ROOT/tools/iso-19450-opl.ebnf"
readonly OPM_DIR="$REPO_ROOT/src/opm"
readonly RB_SCRIPT="$REPO_ROOT/scripts/v6-opl-syntax.rb"

if [ ! -f "$EBNF_FILE" ]; then
    printf 'V6: SKIP (pin file absent: %s)\n' "tools/iso-19450-opl.ebnf"
    exit 0
fi

if [ ! -d "$OPM_DIR" ] || ! find "$OPM_DIR" -maxdepth 2 -name 'opl.txt' -print -quit | grep -q .; then
    printf 'V6: SKIP (source directory absent or empty — no inputs to validate: %s)\n' "src/opm/"
    exit 0
fi

cd "$REPO_ROOT"
exec bundle exec ruby "$RB_SCRIPT" "$EBNF_FILE" "$OPM_DIR"
