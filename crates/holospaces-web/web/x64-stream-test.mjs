// x64-stream-test.mjs — Phase 4 witness: a tab STREAMS a resume, pulling each RAM page one-by-κ
// from a publisher (verify-on-receipt), never holding the whole snapshot. This is the shareable-
// κ-link seam: the publisher shares a small manifest, the adopter fetches only the unique pages
// by κ over a transport (here an in-process callback; in a tab it's content_net / a WebRTC peer).
// A forging transport (attacker bytes for every κ) is refused on receipt (L5).
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const hs = require(path.join(WEB, "pkg-node", "holospaces_web.js"));
const fail = (m) => { console.error("FAIL: " + m); process.exit(1); };

// Publisher: resume the existing κ-blob fixture into a live machine, then seal it for streaming
// (RAM → an internal κ-store of only the unique pages) and publish the small manifest.
const blob = new Uint8Array(await readFile(path.join(WEB, "fixtures", "x64-resume-snapshot.kblob")));
const server = hs.X64Workspace.resume_kappa(blob);
const manifest = server.suspend_kappa_sealed();

// Adopter: stream a resume, fetching each page by κ from the publisher (verified in-Rust on
// receipt). The transport callback is the only thing between them — a tab swaps in content_net.
let fetched = 0, bytes = 0;
const fetch = (k) => {
  const p = server.serve_kappa_page(k);
  if (p) { fetched++; bytes += p.length; }
  return p || null;
};
const t0 = performance.now();
const tab = hs.X64Workspace.resume_kappa_streamed(manifest, fetch);
const tStream = performance.now() - t0;
if (tab.terminal() !== server.terminal()) fail("streamed resume console is not bit-exact");

// Forger: serve attacker bytes for EVERY κ → the resume must be refused on receipt (L5).
let refused = false;
try {
  hs.X64Workspace.resume_kappa_streamed(manifest, (_k) => new Uint8Array(4096).fill(0xaa));
} catch {
  refused = true;
}
if (!refused) fail("a forging transport was NOT refused — verify-on-receipt is broken");

console.log("PASS — x64 κ-snapshot STREAMING resume witness (compiled wasm):");
console.log(`  manifest  : ${(manifest.length / 1024).toFixed(0)} KiB published (state + per-page κ list)`);
console.log(`  streamed  : ${fetched} unique pages = ${(bytes / 1048576).toFixed(1)} MiB pulled by κ (verify-on-receipt) in ${tStream.toFixed(0)} ms`);
console.log("  bit-exact : the streamed machine's console is identical to the publisher's");
console.log("  forger    : REFUSED (attacker bytes rejected on receipt — L5)");
console.log("  → a tab streams only the unique pages by κ from a κ-link, verifying each. Phase 4 seam.");
process.exit(0);
