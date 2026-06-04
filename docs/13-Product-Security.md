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
content yields the same κ and does not grow the store. The deeper dense-matrix
**structural** dedup (cost by unique structure, not bits) and the **decreasing
per-node resource floor** are substrate properties holospaces inherits (hologram
conformance); the holospaces-observable consequence — repetition collapses to a κ
already resolved — is what makes DoS-by-repetition uneconomic here.

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
`sec_confidentiality_content_is_reachable_only_by_its_kappa`. The deeper property
— content is UOR-encoded and meaningful only in the observer's **base-frame**, so
a routing/storage peer without the frame perceives nothing (no ciphertext, no key
to steal) — is the UOR/substrate layer this builds on; the enforced,
holospaces-observable property is **κ-as-capability**.

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

## 13.8 Verification & validation

`CC-40` (`vv/suites/cc40-product-security.sh`) runs the witness battery; the
quality-requirements catalog (chapter 10) lists it `live`. The properties are
enforced *by construction*, so the witnesses assert what the substrate
**refuses** — a tampered byte, an escalated capability, a fabricated κ — rather
than a bolted-on check. A change that weakened any property would fail `CC-40`.
