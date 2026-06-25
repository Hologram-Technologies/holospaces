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
#   CC-* Targets — behavior/test-driven: an executable behavioral spec is written
#        FIRST (vv/targets/), EXPECTED RED, and the component is then built to it.
#        The target tier is NON-GATING (a RED target never fails V&V or blocks
#        deploy); a GREEN target is the signal to PROMOTE its suite to vv/suites/.

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
    if [ "${HOLOSPACES_SKIP_PERF:-0}" = "1" ] && [[ "$name" == perf-* ]]; then
        echo "    SKIP ($name — performance gate runs on release CI)"
        continue
    fi
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

# ── CC-* Targets (BDD: the behavioral spec, written first, expected RED) ──
# Each suite in vv/targets/ is an executable behavioral target for unfinished
# work (the arc42 ch.10 rows marked "target — not yet live"). It is EXPECTED to
# fail until the component is built to it (test-driven). This tier is NON-GATING:
# a RED target does not fail V&V and does not block deploy — it is the spec we
# build toward. A GREEN target is a defect of placement: its component is live, so
# PROMOTE the suite into vv/suites/ (and un-#[ignore] its witness).
echo "── CC-* Targets (behavioral spec first; expected RED until built — non-gating) ──"
target_met=""
target_red=""
if [ -d "$ROOT/vv/targets" ]; then
    for suite in "$ROOT"/vv/targets/*.sh; do
        [ -e "$suite" ] || continue
        name="$(basename "$suite" .sh)"
        echo "  • $name (target)"
        if "$suite" >/dev/null 2>&1; then
            echo "    ⚑ TARGET MET — promote $name → vv/suites/ (the component is now live)"
            target_met="$target_met ${name%%-*}"
        else
            echo "    RED (target — not yet live; build the component to this spec)"
            target_red="$target_red ${name%%-*}"
        fi
    done
fi
[ -n "$target_met$target_red" ] && echo
echo

# ── Portability (arc42 ch.7 deployment view; quality goal Q6) ──
# The same holospaces peer core compiles for every environment hologram
# supports — native, browser (wasm32), and bare-metal (no_std). Mirrors
# hologram's own tri-target discipline (`just wasm` / `just embedded`).
echo "── Portability (the peer builds for browser + bare-metal, ch.7 / Q6) ──"
port_rc=0
if command -v cargo >/dev/null 2>&1 && command -v rustup >/dev/null 2>&1; then
    rustup target add wasm32-unknown-unknown thumbv7em-none-eabi >/dev/null 2>&1 || true
    echo "  • native"
    cargo build --manifest-path "$ROOT/Cargo.toml" -p holospaces >/dev/null 2>&1 || port_rc=1
    echo "  • browser (wasm32-unknown-unknown)"
    cargo build --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --target wasm32-unknown-unknown >/dev/null 2>&1 || port_rc=1
    echo "  • bare-metal (thumbv7em-none-eabi, no_std)"
    cargo build --manifest-path "$ROOT/Cargo.toml" -p holospaces \
        --no-default-features --target thumbv7em-none-eabi >/dev/null 2>&1 || port_rc=1
    [ "$port_rc" -eq 0 ] && echo "  PASS (native · browser · bare-metal)" || echo "  FAIL"
else
    echo "  SKIP — cargo/rustup not available"
fi
echo

# CC rows defined in the catalog (arc42 ch.10); those without a green suite
# above are not-yet-witnessed.
all_cc="CC-1 CC-2 CC-3 CC-4 CC-5 CC-6 CC-7 CC-8 CC-9 CC-10 CC-11 CC-12 CC-13 CC-14 CC-15 CC-16 CC-17 CC-18 CC-19 CC-20 CC-21 CC-22 CC-23 CC-24 CC-25 CC-26 CC-27 CC-28 CC-29 CC-30 CC-31 CC-32 CC-33 CC-34 CC-35 CC-36 CC-37 CC-38 CC-39 CC-40 CC-41 CC-42 CC-43 CC-44 CC-45 CC-46 CC-48 CC-49 CC-50 CC-51"
# Targets (arc42 ch.10 rows marked "target — not yet live"): unfinished work with
# a behavioral spec in vv/targets/ but not yet a live component. Reported, never gated.
# (CC-44/45/46/48/49/50 were promoted to live — their suites are in vv/suites/ and gated.)
target_cc="CC-47 CC-51"
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
echo "CC targets:  ${target_cc}  — behavioral specs in vv/targets/ (BDD: build each to green, then promote)"
[ -n "$target_met" ] && echo "CC targets MET (promote → vv/suites/):${target_met}"
[ "$port_rc" -eq 0 ] && echo "Portability  PASS  (native · browser · bare-metal peer builds)" || echo "Portability  FAIL"
echo
if [ "$spec_rc" -eq 0 ] && [ "$cc_rc" -eq 0 ] && [ "$port_rc" -eq 0 ]; then
    echo "V&V: GREEN (specification conformance + all implemented components + portability)."
else
    echo "V&V: FAILED."
fi
[ "$spec_rc" -eq 0 ] && [ "$cc_rc" -eq 0 ] && [ "$port_rc" -eq 0 ]
exit $?
