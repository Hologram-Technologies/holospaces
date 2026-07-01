// x64-link-resume-test.mjs — THE ONE-LINK SEAM: a single κ-link (`holo://#k=<κ>`) opens a live
// machine. The link carries ONLY the manifest's own κ (one hash). The adopter fetches the manifest
// by that κ (verified, L5), then streams the unique RAM pages by κ (each verified on receipt, L5),
// and resumes a BIT-EXACT live machine — nothing else is shared between publisher and adopter.
// A forged manifest, or a transport that serves attacker bytes, is refused. This is the headless
// proof of "paste a link → land in a running computer"; a tab swaps the in-process callback for
// content_net / a WebRTC peer and reads the κ from `location.hash`.
import { readFile } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const hs = require(path.join(WEB, "pkg-node", "holospaces_web.js"));
const fail = (m) => { console.error("FAIL: " + m); process.exit(1); };

// ── PUBLISHER ── a live machine → seal it → content-address the manifest into ONE κ (the link). ──
const blob = new Uint8Array(await readFile(path.join(WEB, "fixtures", "x64-resume-snapshot.kblob")));
const server = hs.X64Workspace.resume_kappa(blob);
const manifest = server.suspend_kappa_sealed();   // RAM → an internal κ-store of unique pages
const manifestKappa = hs.kappa(manifest);          // the single hash the URL carries
const link = `holo://#k=${manifestKappa}`;

// ── TRANSPORT ── one content store the adopter pulls EVERYTHING from by κ: the manifest AND the
// pages. The callback is the only channel between the two (a tab: content_net / WebRtcLink). ──
let fetched = 0, bytes = 0;
const fetchByKappa = (k) => {
  if (k === manifestKappa) return manifest;        // the manifest object itself
  const p = server.serve_kappa_page(k);            // a unique RAM page
  if (p) { fetched++; bytes += p.length; }
  return p || null;
};

// ── ADOPTER ── paste the link → resume. Nothing else is shared. ──
const k = new URL(link).hash.replace(/^#k=/, "");  // parse the κ out of the link
if (k !== manifestKappa) fail("link parse");
const gotManifest = fetchByKappa(k);
if (!hs.verify_kappa(gotManifest, k)) fail("the manifest does not re-derive to its κ (L5)");
const t0 = performance.now();
const tab = hs.X64Workspace.resume_kappa_streamed(gotManifest, fetchByKappa);
const ms = performance.now() - t0;
if (tab.terminal() !== server.terminal()) fail("the link-resumed console is not bit-exact");

// ── FORGER 1 ── a tampered manifest is refused before a single page is touched. ──
const forged = new Uint8Array(manifest);
forged[100] ^= 0xff;
if (hs.verify_kappa(forged, manifestKappa)) fail("a forged manifest passed κ verification");

// ── FORGER 2 ── a transport that serves attacker bytes for every page κ is refused on receipt. ──
let refused = false;
try {
  hs.X64Workspace.resume_kappa_streamed(gotManifest, (kk) =>
    kk === manifestKappa ? manifest : new Uint8Array(4096).fill(0xaa),
  );
} catch {
  refused = true;
}
if (!refused) fail("a forging transport was not refused");

console.log(
  "PASS — x64 ONE-LINK resume (compiled wasm):\n" +
    `  link      : holo://#k=${manifestKappa.slice(0, 16)}…  (one κ, ${manifestKappa.length} chars)\n` +
    `  manifest  : ${(manifest.length / 1024).toFixed(0)} KiB — fetched by its κ + VERIFIED (L5)\n` +
    `  streamed  : ${fetched} unique pages = ${(bytes / 1048576).toFixed(1)} MiB by κ, verify-on-receipt, in ${ms.toFixed(0)} ms\n` +
    "  bit-exact : the link-opened machine's console === the publisher's\n" +
    "  forger    : REFUSED — a tampered manifest AND a forging transport both rejected (L5)\n" +
    "  → paste ONE κ-link, a live machine reconstructs from κ. The shareable-computer seam.",
);
