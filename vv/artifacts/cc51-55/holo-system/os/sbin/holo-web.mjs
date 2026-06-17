// holo-web.mjs — the OPEN-WEB κ transport. The internet is a giant HYPERGRAPH: every object is a NODE
// addressed by its self-verifying attributes (its κ), and an HTTPS IRI, an IPFS CID, a holo:// alias and
// a did:holo are all EDGES that point to the same node. This transport follows the HTTPS edges — an
// object's own W3C DID Core `alsoKnownAs` IRIs and any content-address GATEWAY that serves the κ-route
// (/.holo/<axis>/<hex>, did:holo's analogue of an IPFS Trustless Gateway) — so the healer (and any agent)
// can ACCESS and IMPORT any object on the open internet, not just the origin it booted from.
//
// Trust is in the math, not the source (Law L5): every byte is RE-DERIVED to the address before it is
// admitted, so fetching from ANY server — a stranger's CDN, a mirror, a gateway — is safe; a wrong or
// hostile byte simply loses. This is what lets the substrate treat the whole web as one address space:
// the κ is the identity, the URL is merely a place a copy happens to live.
//
// 100% standards-native: W3C DID Core (`alsoKnownAs` equivalence edges), W3C Subresource Integrity +
// multiformats multihash (the κ = sha-256 IS a CIDv1, so the SAME hex resolves on IPFS too), WHATWG Fetch,
// RFC 8615 well-known discovery, JSON-LD / Linked Data objects. Pure + isomorphic (browser & Node fetch);
// its one import is the resolver's re-derivation primitive — one canonical hash, no per-surface drift (L2).

import { reDerive, hexOf } from "./holo-resolver.mjs";

// candidateUrls(κ, { gateways, iris }) → string[] — every HTTPS edge that may point to this κ-node:
//   • iris(κ) → [url]   the object's advertised W3C DID Core `alsoKnownAs` HTTPS IRIs (follow the edge)
//   • gateways          content-address gateways that serve the κ-route: <gateway>/.holo/sha256/<hex>
// De-duplicated, http(s) only. The order is preference (object's own IRIs first, then shared gateways).
export async function candidateUrls(kappa, { gateways = [], iris = null } = {}) {
  const hex = hexOf(kappa);
  const urls = [];
  if (typeof iris === "function") { try { for (const u of (await iris(kappa)) || []) if (u) urls.push(String(u)); } catch { /* iris is best-effort */ } }
  for (const g of gateways) urls.push(String(g).replace(/\/$/, "") + "/.holo/sha256/" + hex);
  return [...new Set(urls.filter((u) => /^https?:\/\//.test(u)))];
}

// webSource(cfg) → async (κ) → Uint8Array | null — a source for the resolver chain. Try each HTTPS edge
// and return the FIRST byte-set that RE-DERIVES to the κ (Law L5). A wrong/hostile server is refused; if
// no edge holds it → null (the chain falls through to IPFS / mesh / origin). `.peer` labels provenance.
export function webSource({ gateways = [], iris = null, fetchImpl, label = "web" } = {}) {
  const f = fetchImpl || (typeof fetch !== "undefined" ? fetch : null);
  const s = async (kappa) => {
    if (!f) return null;
    const hex = hexOf(kappa);
    for (const u of await candidateUrls(kappa, { gateways, iris })) {
      try {
        const r = await f(u, { redirect: "follow" });
        if (!r || !r.ok) continue;
        const b = new Uint8Array(await r.arrayBuffer());
        if ((await reDerive(b)) === hex) return b;          // self-verifying: only a copy that hashes to the κ is admitted
      } catch { /* unreachable / CORS / wrong → try the next edge */ }
    }
    return null;
  };
  s.peer = label;
  return s;
}

// importObject(ref, cfg) → { kappa, bytes } | null — IMPORT ANY OBJECT from the open internet, verified.
//   • ref is a κ / did:holo  → resolve it from the object's IRIs + gateways and re-derive (content lookup).
//   • ref is an HTTPS URL    → fetch it and DERIVE its κ — content-addressing an arbitrary web object so it
//                              can then live on the substrate (be pinned, shared by κ, healed). Either way
//                              the returned bytes are self-verifying: kappa === did:holo:sha256:H(bytes).
// This is the bridge that makes the open web part of the UOR address space — agents and humans alike can
// pull any node into the substrate by following one edge, and from then on it is content-addressed.
export async function importObject(ref, { gateways = [], iris = null, fetchImpl } = {}) {
  const f = fetchImpl || (typeof fetch !== "undefined" ? fetch : null);
  if (!f || !ref) return null;
  if (/^https?:\/\//.test(String(ref))) {                   // import by URL → derive its content address
    try {
      const r = await f(ref, { redirect: "follow" });
      if (!r || !r.ok) return null;
      const b = new Uint8Array(await r.arrayBuffer());
      return { kappa: "did:holo:sha256:" + (await reDerive(b)), bytes: b };
    } catch { return null; }
  }
  const b = await webSource({ gateways, iris, fetchImpl: f })(ref);   // import by κ → resolve + verify
  return b ? { kappa: "did:holo:sha256:" + hexOf(ref), bytes: b } : null;
}
