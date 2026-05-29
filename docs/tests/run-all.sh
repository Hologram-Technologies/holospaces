#!/usr/bin/env bash
#
# tests/run-all.sh
#
# Runs the unit tests in tests/. Exercises the translator and extractor
# library code (scripts/lib/*.rb) against synthetic fixtures with no
# dependency on the eventual transcribed-standard pin files. Tests that
# the V6/V7 infrastructure works end-to-end before any pin file lands.
#
# Run directly:
#
#   ./tests/run-all.sh

set -euo pipefail

readonly REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

failed=0
for test in tests/test-*.rb; do
    printf '%-40s ' "$(basename "$test")"
    if bundle exec ruby "$test" >/dev/null 2>&1; then
        printf 'PASS\n'
    else
        printf 'FAIL\n'
        bundle exec ruby "$test" 2>&1 | sed 's/^/    /'
        failed=$((failed + 1))
    fi
done

if [ "$failed" -gt 0 ]; then
    printf '\n%d test file(s) failed\n' "$failed" >&2
    exit 1
fi
printf '\nall tests passed\n'
