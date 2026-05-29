#!/usr/bin/env bash
#
# v8-iso-15288-superset.sh
#
# For every process named in tools/iso-15288-processes.txt, asserts a
# section heading with that name appears in the corresponding
# src/15288/<process-group>.adoc file. Structural superset rule (the
# wiki may add sub-sections beyond those the standard names; it must
# not omit any).
#
# Reconciliation gap discipline:
#   - If tools/iso-15288-processes.txt is absent, skip with reason.
#   - If src/15288/ is absent or empty, skip with reason.

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly PROCESSES_FILE="$REPO_ROOT/tools/iso-15288-processes.txt"
readonly LIFECYCLE_DIR="$REPO_ROOT/src/15288"
readonly RB_SCRIPT="$REPO_ROOT/scripts/v8-iso-15288-superset.rb"

if [ ! -f "$PROCESSES_FILE" ]; then
    printf 'V8: SKIP (pin file absent: %s)\n' "tools/iso-15288-processes.txt"
    exit 0
fi
if [ ! -d "$LIFECYCLE_DIR" ] || ! find "$LIFECYCLE_DIR" -maxdepth 1 -name '*.adoc' -print -quit | grep -q .; then
    printf 'V8: SKIP (source directory absent or empty — no inputs to validate: %s)\n' "src/15288/"
    exit 0
fi

cd "$REPO_ROOT"
exec bundle exec ruby "$RB_SCRIPT" "$PROCESSES_FILE" "$LIFECYCLE_DIR"
