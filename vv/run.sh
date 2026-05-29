#!/usr/bin/env bash
#
# vv/run.sh — the holospaces V&V runner (single entry point; also `just vv`).
#
# Evaluates holospaces against external authoritative standards, as defined by
# the documentation (arc42 chapter 10: "Verification and Validation" + the
# Conformance catalog). This script IMPLEMENTS that definition; the docs are
# the source of truth.
#
# Tiers:
#   CS-* Specification conformance — the documentation vs arc42 / OPM ISO 19450
#        / ISO/IEC/IEEE 15288, via validators V1–V8. Live; run here.
#   CC-* Component conformance — each component vs its external authority. Each
#        suite is added here as its component is implemented (conformance-driven).

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

echo "═══ holospaces V&V ═══"
echo
echo "── CS-* Specification conformance (docs vs arc42 / OPM ISO 19450 / ISO 15288) ──"
"$ROOT/docs/scripts/build.sh"
spec_rc=$?
echo

# CC-* component suites are registered here as components land, e.g.:
#   "$ROOT/vv/suites/cc1-kappa-addressing.sh"   # κ-labels vs imported hash KATs
# Until a component exists, its CC-* row in the catalog is the authoritative
# requirement and is reported as not-yet-witnessed (not a failure: there is no
# component to witness). Adding a component without its CC-* witness is a defect.
cc_pending="CC-1 CC-2 CC-3 CC-4 CC-5"

echo "── Summary ──"
if [ "$spec_rc" -eq 0 ]; then
    echo "CS-1..CS-6  PASS  (specification conforms to its external standards)"
else
    echo "CS-1..CS-6  FAIL  (see the build output above)"
fi
echo "CC-* ($cc_pending)  not yet witnessed — no component implemented; authorities defined in the catalog + PROVENANCE.md"
echo
[ "$spec_rc" -eq 0 ] && echo "V&V: specification conformance GREEN." || echo "V&V: FAILED."
exit "$spec_rc"
