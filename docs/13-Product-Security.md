# 13. Product Security & Threat Model

This chapter is the product-security and threat-modeling authority for
holospaces. Its purpose is not to *describe* a security posture but to make one
**enforced and proven**: every security property stated here has a
verification-and-validation witness in code (`CC-40`,
`crates/holospaces/tests/cc40_product_security.rs`,
`vv/suites/cc40-product-security.sh`), so a claim that holospaces holds a
property is a claim the test battery refuses to let regress.

holospaces is **UOR-native** — built on the hologram substrate, content-
addressed, trust-less, and able to run on bare metal. Its security properties
are therefore *intrinsic to the model*, not a perimeter bolted around it. The
threat model is best understood by **dropping the internet's assumptions**
(trusted servers, transport encryption over plaintext, per-request cost) and
reasoning from the substrate's own primitives.

## 13.1 Security posture (what is different)

| Property | The internet's model | holospaces (UOR-native) |
|---|---|---|
| **Integrity / authenticity** | TLS + a trusted server you must not be MITM'd from | Content **is** its κ; the receiver re-derives every byte (Law L5). Forgery is structurally impossible — there is no trusted intermediary to subvert. |
| **Confidentiality** | Encrypt plaintext under a key an attacker may try to steal/break | Content is UOR-encoded, **perceivable only in the observer's base-frame**; the κ is the capability to address it. A router/store without the frame perceives nothing — no key-attack surface. |
| **Authority** | Ambient authority + ACLs checked at each call | **Object-capabilities**: a holospace runs under exactly its κ-addressed capability set; authority can only be *attenuated*, never escalated (no confused deputy, no ambient escape). |
| **Identity** | A server account in a central database | **Self-sovereign**: a content-addressed identity (`CC-1`); no central account to breach, none to forge without the key. |
| **Cost / availability** | Per-request, per-byte; more clients → more load → congestion; DoS by repetition | The hologram **dense-matrix hierarchy** deduplicates by *unique structure*, not bits; the per-node resource floor **decreases** as the network grows. Repetition collapses; DoS economics invert. |

## 13.2 Assets

- **The operator's content** — holospaces, configurations, workspace files,
  secrets, dotfiles — held as κ-addressed content in the store (Law L3).
- **The operator's identity** — a self-sovereign key and the κ it addresses.
- **Authority** — the capability sets that bound what a holospace may do.
- **Availability** — the ability to resolve content and to reach the network
  (and, for a sandboxed peer, an egress route to the legacy internet).

## 13.3 Trust boundaries and the peer taxonomy

Two peer classes carry different boundaries:

| | NIC? | Egress | Content | Trust boundary |
|---|---|---|---|---|
| **Sovereign** (bare-metal holospace — e.g. a flashed device, a node, Chromium-on-device) | yes | direct | direct + serves the mesh | speaks to the legacy internet *and* the hologram network with no intermediary |
| **Sandboxed** (browser tab — e.g. a Chromebook) | no | routes through a sovereign peer | over the mesh | compute is local; the network is reached through peers |

Egress-via-a-peer is therefore a property of the **browser sandbox**, not of
holospaces. As sovereign peers multiply, **no operator depends on any specific
egress peer** — egress is a commodity mesh service, and the egress-trust surface
*shrinks* with network growth.

The **cold-start gateway** (the GitHub Pages site) is an explicitly *untrusted*
boundary (ADR-005): it may serve any bytes, but every byte is verified by
re-derivation on load, so a compromised CDN cannot inject content a peer accepts.

## 13.4 Adversaries

- **A malicious peer / node** (egress, content, or storage) — may drop, delay,
  observe, or attempt to forge.
- **A compromised cold-start gateway / CDN** — may serve arbitrary bytes.
- **A passive network observer.**
- **A malicious guest / holospace** — may attempt to exceed its authority.
- **A Sybil / eclipse adversary** — may flood the network with peers to distort
  a victim's view.
- **A key thief** — may obtain an operator's self-sovereign key.

## 13.5 Security requirements (each enforced + proven)

Each requirement names its enforcement and its `CC-40` witness.

### SEC-1 — Integrity: forgery is structurally impossible

Content is its κ; any byte that does not re-derive to the requested κ is refused
on receipt (Law L5, SPINE-4). A malicious peer, gateway, or observer cannot
substitute or tamper content — there is no trusted intermediary to subvert.
*Enforced by* re-derivation at every boundary (import `CC-20`, the content
network `CC-38`, the browser `Console::receive`).
*Witnesses:* `sec_integrity_tampered_content_does_not_re_derive` (a single
tampered bit fails verification), `sec_integrity_the_network_refuses_a_forging_responder`
(the content network never fabricates content for an unheld κ).

### SEC-2 — Authority: object-capabilities, no ambient authority

A holospace runs under exactly its κ-addressed capability set; authority can only
be attenuated. *Enforced by* `Capabilities::admits` (subset of roots/channels;
budget containment under the 0 = unbounded convention; a flag granted only if the
parent holds it). *Witness:*
`sec_authority_capabilities_only_attenuate_never_escalate` — a lesser set is
admitted; every escalation vector (a flag the parent lacks, a wider quota, an
unbounded budget under a bounded parent, a foreign storage root) is refused.

### SEC-3 — Cost / dedup: identical content resolves once

The UOR cost model resolves content **once** and shares it; idempotent `put` is
its holospaces-observable form. *Enforced by* the content-addressed store.
*Witness:* `sec_cost_identical_content_deduplicates` — re-storing identical
content yields the same κ and does not grow the store. Content also has **one
identity network-wide** — every peer computes the same κ for the same bytes — so
the same artifact is *the same content everywhere*, resolved once and shared, not
re-identified per peer (*witness:* `sec_cost_content_has_one_identity_on_every_peer`).
The deeper dense-matrix **structural** dedup (cost by unique structure, not bits)
and the **decreasing per-node resource floor** are substrate properties holospaces
inherits (hologram conformance, §13.9); the holospaces-observable consequence —
repetition collapses to a κ already resolved, and the same κ on every node — is
what makes DoS-by-repetition uneconomic and the network cheaper-with-scale.

### SEC-4 — Identity: self-sovereign and unforgeable

An operator is a content-addressed identity (`CC-1`), deterministic from the key;
a roster is content-addressed and binds its operator. *Enforced by* the σ-axis
address of the key and the `Realization` form of the roster. *Witness:*
`sec_identity_is_self_sovereign_and_unforgeable` — the same key is the same
identity, a different key a different one, and a roster round-trips to the same κ
bound to its operator (it cannot be forged for another).

### SEC-5 — Confidentiality: the κ is the capability to perceive

Content is addressable — hence perceivable — only via its κ; a peer cannot
enumerate or fabricate content it was not given the κ for. *Enforced by* the
content-addressed store (an unknown κ is absent). *Witness:*
`an_absent_kappa_resolves_to_none`. This — together with
content-blind intermediaries (`SEC-7`) — **meets confidentiality for the deployed
architecture**, and the reason is structural, not a deferral: holospaces has **no
server** (ADR-001/Law L4), so no *untrusted party* ever holds perceivable content.
Compute runs in the peer that owns the holospace; routing/storage peers move
content addressed by κ (an unknown κ is absent) and exits forward opaque bytes
(`SEC-7`) — none of them *perceive* or *operate on* content.

A *deeper* property — content UOR-encoded so a **storage peer holding the bytes**
perceives nothing without the observer's **base-frame** — would be realized by
**frame-relative-at-rest encoding** (`uor-prism-crypto`). holospaces does not
compose it and **does not need to today**: storage is the operator's own peer
(OPFS) or a **node the operator owns** (`CC-39`), so there is no untrusted holder
to defend against. It is a **conditional** item (§13.9), admitted only if the
threat model later adds untrusted storage peers — not a standing requirement.
**Homomorphic encryption (`uor-prism-fhe`) is *not needed at all*:** FHE secures
an *untrusted party's computation* over content it cannot perceive, and holospaces
has no untrusted computation. The property holospaces enforces today —
**κ-as-capability + content-blindness** — is sufficient for SEC-5.

### SEC-6 — Reference resolution: verified against the κ, not the reference

"this URL / name → this κ" is the trust-sensitive boundary (ADR-013): the located
reference is the *request*; the κ is the *identity*. Whatever a reference points
at, the content is **verified by re-derivation against the κ on the κ's own axis**
before it is accepted — an OCI `sha256:` digest *is* a κ on the `sha256` axis
(`CC-10`/`CC-20`), so a tampered blob is refused regardless of where the reference
led. *Enforced by* `verify_kappa_axis` at the import boundary. *Witness:*
`sec_reference_resolution_verifies_against_the_kappa_on_its_axis`.

### SEC-7 — Egress boundary: the exit is content-blind

An egress peer forwards a guest's payload as **opaque bytes** and never perceives
or alters it. The node holds only sockets — no `KappaStore`, no identity, no
base-frame — so it is a pipe, not an observer. *Enforced by* the `EgressServer`'s
structure (it forwards bytes it cannot interpret). *Witness:*
`the_egress_forwards_opaque_content_without_perceiving_it` (`CC-39`,
`crates/holospaces-node`) — an arbitrary binary payload (every byte value,
including bytes resembling frame opcodes or κ-content) is delivered byte-identical
through the node.

### SEC-8 — Resource bounds: untrusted input cannot exhaust a peer

A malicious or buggy counterparty — a peer that forges a canonical κ/config, a
guest that issues a malformed or oversized device request, a remote that floods
connections — must not crash a holospaces peer or balloon its memory. Every
allocation driven by untrusted input is **bounded by the actual payload or a
fixed quota, never by a declared count**, and the substrate is the memory (the
`KappaStore` backs the κ-disk; RAM holds only bounded caches; rootfs assembly is
streamed, never materialized dense). *Enforced by:* peer-supplied
`Vec::with_capacity` reservations bounded by the remaining payload
(`realizations.rs`, `config.rs`); the `virtio-9p` workspace rejecting a
count/body mismatch and bounding `offset+count` (checked arithmetic) against a
per-workspace quota; the NAT's connection table bounded + idle-reaped with
per-connection buffers capped behind a shrinking advertised window; the κ-disk
read-through cache bounded (single-entry eviction); and ext4 assembly streamed
into the `KappaStore` (no dense full-image buffer). *Witnesses:*
`forged_huge_ref_count_errors_without_ballooning`,
`forged_huge_directive_count_errors_without_ballooning`,
`twrite_with_mismatched_count_errors_not_panics`,
`twrite_at_huge_offset_is_quota_rejected`, `the_connection_table_is_bounded`,
`from_guest_backpressure_shrinks_the_advertised_window`,
`read_cache_is_bounded_and_never_corrupts_reads`,
`streamed_layers_overlay_is_identical_to_all_at_once`.

## 13.6 The legacy-internet boundary (named, not wished away)

The UOR-native properties hold **inside** the holospaces network. An egress
peer's **outside** leg still touches the legacy internet: the destination host
sees the exit peer's IP and speaks legacy TLS to it. Frame-relative perception
and dedup are network-internal; the egress is a *translation point* where
old-world properties reappear on the outside leg only. The threat model states
this precisely so it is not mistaken for a hole in the inside model: an egress
peer forwards content it cannot perceive (frame-relative), but the legacy host at
the far end observes what any legacy server observes of its own clients.

## 13.7 Residual threats

- **Availability / eclipse.** Integrity always holds (verify-on-receipt), but a
  Sybil/eclipse adversary can degrade *availability and routing*. The dense-matrix
  cost model and the decreasing per-node floor make flooding uneconomic; eclipse
  resistance is a substrate (DHT) property holospaces inherits.
- **Reference resolution is the trust-sensitive boundary.** Content transfer is
  trustless, but "this URL / name → this κ" is where a wrong answer hands you
  malicious-but-validly-addressed content. The located reference is the *request*;
  the κ is the *identity* (ADR-013). Trust decisions live at that binding.
- **Key compromise.** Self-sovereign means self-responsible: a stolen key
  compromises its identity and capabilities. There is, by design, no central
  recovery — the mitigation is key custody, not a server.

## 13.8 Adversary → defense

| Adversary | Defeated by | Residual |
|---|---|---|
| Malicious peer / node (forge, tamper) | SEC-1 (re-derivation), SEC-7 (egress content-blind), SEC-8 (forged-count allocations bounded) | may withhold/delay (availability) |
| Compromised cold-start gateway / CDN | SEC-1 (every byte re-derived on load) | availability only |
| Passive observer / routing peer | SEC-5 (κ-as-capability; frame at Prism) | network metadata (timing/volume); legacy-internet destinations on the egress outside leg (§13.6) |
| Malicious guest / holospace | SEC-2 (capabilities only attenuate), SEC-8 (9p quota + bounded device buffers) | resource use bounded by the workspace quota |
| Resource exhaustion / DoS (oversized or forged input, connection flood) | SEC-8 (allocations bounded by payload/quota, NAT backpressure + bounded reaped table, streamed assembly) | throughput may degrade; the peer never crashes or OOMs |
| Sybil / eclipse | SEC-1 holds regardless | availability/routing (substrate DHT, §13.9) |
| Key thief | — | self-sovereign = self-responsible (key custody) |

## 13.9 Enforcement layering (what is proven where)

The threat model spans three layers; this chapter is explicit about which holds
each property so nothing is over-claimed:

- **holospaces-enforced + witnessed here** (`CC-40`/`CC-39`): SEC-1 integrity,
  SEC-2 authority, SEC-3 dedup (idempotent + network-wide identity), SEC-4
  identity, SEC-5 κ-as-capability, SEC-6 reference resolution, SEC-7 egress
  content-blindness, SEC-8 resource bounds (DoS resistance).
- **Substrate-inherited** (hologram conformance): the dense-matrix **structural**
  dedup (cost by unique structure, not bits), the **decreasing per-node resource
  floor**, and DHT eclipse/Sybil resistance. holospaces witnesses their
  *observable consequences* (one κ per content, repetition collapses) but does not
  re-prove the substrate.
- **Out of scope by analysis (not a gap)**: *homomorphic* confidentiality
  (`uor-prism-fhe`) is **not needed**. FHE secures an *untrusted party's
  computation* over content it cannot perceive; holospaces has none — there is no
  server (ADR-001/Law L4), compute runs in the holospace-owning peer, and
  intermediaries route/store (content-blind, `SEC-7`) but never *operate on*
  content. *Frame-relative-at-rest* encoding (`uor-prism-crypto`) is a
  **conditional** future item — relevant only if the threat model later admits
  **untrusted storage peers** (strangers caching your κ). The deployed model
  stores on the operator's own peer/node (`CC-39`), so κ-as-capability +
  content-blindness already satisfy SEC-5; if that threat is admitted, the answer
  is frame-relative encoding (**not** FHE), with a witness added then.

## 13.10 Verification & validation

`CC-40` (`vv/suites/cc40-product-security.sh`) + `CC-39`
(`vv/suites/cc39-node-egress.sh`) run the witness battery; the
quality-requirements catalog (chapter 10) lists them `live`. The properties are
enforced *by construction*, so the witnesses assert what the substrate
**refuses** — a tampered byte, an escalated capability, a fabricated κ — rather
than a bolted-on check. A change that weakened any property would fail the gate.

| Req | Property | Witness | Suite |
|---|---|---|---|
| SEC-1 | Integrity (no forgery) | `sec_integrity_tampered_content_does_not_re_derive`, `sec_integrity_the_network_refuses_a_forging_responder` | `CC-40` |
| SEC-2 | Authority (attenuation only) | `sec_authority_capabilities_only_attenuate_never_escalate` | `CC-40` |
| SEC-3 | Cost / dedup (one κ, network-wide) | `sec_cost_identical_content_deduplicates`, `sec_cost_content_has_one_identity_on_every_peer` | `CC-40` |
| SEC-4 | Identity (self-sovereign) | `sec_identity_is_self_sovereign_and_unforgeable` | `CC-40` |
| SEC-5 | Confidentiality (κ-as-capability) | `an_absent_kappa_resolves_to_none` | `CC-40` |
| SEC-6 | Reference resolution (verified vs κ) | `sec_reference_resolution_verifies_against_the_kappa_on_its_axis` | `CC-40` |
| SEC-7 | Egress boundary (content-blind) | `the_egress_forwards_opaque_content_without_perceiving_it` | `CC-39` |
| SEC-8 | Resource bounds (DoS resistance) | `forged_huge_ref_count_errors_without_ballooning`, `forged_huge_directive_count_errors_without_ballooning`, `twrite_with_mismatched_count_errors_not_panics`, `twrite_at_huge_offset_is_quota_rejected`, `the_connection_table_is_bounded`, `from_guest_backpressure_shrinks_the_advertised_window`, `read_cache_is_bounded_and_never_corrupts_reads`, `streamed_layers_overlay_is_identical_to_all_at_once` | Rust quality gate (lib tests) |
