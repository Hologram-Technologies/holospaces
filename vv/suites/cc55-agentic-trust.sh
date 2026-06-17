#!/usr/bin/env bash
# cc-agentic.sh — Component conformance: the Agentic-Web trust face (S6). Carries three catalog rows.
#
# AUTHORITY (external, per holospaces vv/ discipline):
# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system· CC-owl — W3C OWL 2 RL profile rules (https://www.w3.org/TR/owl2-profiles/#OWL_2_RL): TransitiveProperty,
# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system  SymmetricProperty, inverseOf, equivalentClass; run through the same proof-carrying fixpoint as RDFS, so
# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system  each derived triple traces to source κ (Law L5).
# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system· CC-did — W3C DID Core (https://www.w3.org/TR/did-core/): the did:holo method resolves a κ to a
# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system  structurally valid DID Document (self-controlled — authority is the κ, not a registry).
# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system· CC-vc  — W3C VC Data Model 2.0 + VC Data Integrity eddsa-jcs-2022
# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system  (https://www.w3.org/TR/vc-data-model-2.0/, https://www.w3.org/TR/vc-di-eddsa/): JCS canonicalization
# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system  (the substrate's canonical form, Law L2) + ed25519; tamper/wrong-key are refused; a VC bound into a UOR
# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system  object's trust face is doubly protected (VC signature AND the object's own κ).
# Full OWL 2 / DID / VC test suites attach at promotion; this row witnesses the verified trust layer.
#
# This wraps the deterministic Node witness, which IS the behavioral definition of these rows.
# Upstreams to: holospaces  vv/suites/cc-agentic.sh holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system(catalog rows CC-owl, CC-did, CC-vc, arc42 ch.10).
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SYSTEM="${HOLO_SYSTEM:-$(cd "$HERE/../artifacts/cc51-55/holo-system" 2>/dev/null && pwd)}" holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system# holospaces vv/: export HOLO_SYSTEM=<pinned HOLOGRAM>/holo-os/system
command -v node >/dev/null 2>&1 || { echo "CC-owl/did/vc: SKIP — node not available in this environment" >&2; exit 127; }
echo "CC-owl/CC-did/CC-vc — OWL 2 RL entailment + did:holo DID Documents + Verifiable Credentials (Law L5 trust face)"
WITNESS="$SYSTEM/tools/holo-agentic-witness.mjs"
[ -f "$WITNESS" ] || { echo "CC-owl/did/vc: SKIP — witness not found (set HOLO_SYSTEM to a HOLOGRAM holo-os/system checkout)" >&2; exit 127; }
node "$WITNESS"
