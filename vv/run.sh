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

echo "── CC-* Component conformance (components vs their external authorities) ──"
# Each suite in vv/suites/ witnesses one implemented component against its
# imported external authority (provenance in vv/PROVENANCE.md). A component
# whose CC-* row has no suite is not yet implemented: it is reported as
# not-yet-witnessed (the catalog row is the authoritative requirement), not a
# failure. Adding a component without its CC-* witness is a defect.
cc_rc=0
witnessed=""
for suite in "$ROOT"/vv/suites/*.sh; do
    [ -e "$suite" ] || continue
    name="$(basename "$suite" .sh)"
    echo "  • $name"
    "$suite"
    rc=$?
    if [ "$rc" -eq 0 ]; then
        witnessed="$witnessed ${name%%-*}"
    else
        echo "    FAILED ($name, rc=$rc)"
        cc_rc=1
    fi
done
[ -n "$witnessed" ] || witnessed=" (none yet)"
echo

# CC rows defined in the catalog (arc42 ch.10); those without a green suite
# above are not-yet-witnessed.
all_cc="CC-1 CC-2 CC-3 CC-4 CC-5"
pending=""
for cc in $all_cc; do
    cc_key="${cc//-/}"        # CC-1 -> CC1
    cc_key="${cc_key,,}"      # CC1  -> cc1 (matches suite prefix)
    case " $witnessed " in
        *" $cc_key "*) : ;;
        *) pending="$pending $cc" ;;
    esac
done

echo "── Summary ──"
if [ "$spec_rc" -eq 0 ]; then
    echo "CS-1..CS-6   PASS  (specification conforms to its external standards)"
else
    echo "CS-1..CS-6   FAIL  (see the build output above)"
fi
echo "CC witnessed:$witnessed  (component(s) validated against imported authorities)"
echo "CC pending: ${pending:- none}  — not yet implemented; authorities defined in the catalog + PROVENANCE.md"
echo
if [ "$spec_rc" -eq 0 ] && [ "$cc_rc" -eq 0 ]; then
    echo "V&V: GREEN (specification conformance + all implemented components)."
else
    echo "V&V: FAILED."
fi
[ "$spec_rc" -eq 0 ] && [ "$cc_rc" -eq 0 ]
exit $?
