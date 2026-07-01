// make-link-bundle.mjs — PUBLISH a shareable κ-link. Takes a live machine (here the resume
// fixture), seals it, and writes the manifest + every unique RAM page as content-addressed files
// `<out>/k/<safe(κ)>` — a static directory anyone can serve. Prints the ONE-LINE κ-link
// `holo://#k=<manifest-κ>`. Opening that link (resume-link.html) fetches the manifest by its κ
// and streams the pages by κ (verify-on-receipt) → a live machine. The publish side is fully
// verifiable here; the loader's identical logic is proven by `x64-link-resume-test.mjs`.
import { readFile, writeFile, mkdir, rm } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const hs = require(path.join(WEB, "pkg-node", "holospaces_web.js"));
const safe = (k) => k.replace(/[:/]/g, "_");
const OUT = path.join(WEB, "link-bundle");

// PUBLISH: a live machine → seal → manifest → content-address everything into <out>/k/.
// Optional arg: the κ-blob fixture to publish (default: the mid-boot resume fixture).
const FIXTURE = process.argv[2] || path.join(WEB, "fixtures", "x64-resume-snapshot.kblob");
const blob = new Uint8Array(await readFile(path.isAbsolute(FIXTURE) ? FIXTURE : path.join(WEB, FIXTURE)));
const server = hs.X64Workspace.resume_kappa(blob);
const manifest = server.suspend_kappa_sealed();
const manifestKappa = hs.kappa(manifest);
const pageKappas = hs.kappa_manifest_pages(manifest);

await rm(OUT, { recursive: true, force: true });
await mkdir(path.join(OUT, "k"), { recursive: true });
await writeFile(path.join(OUT, "k", safe(manifestKappa)), manifest); // the manifest, by its κ
let bytes = manifest.length;
for (const k of pageKappas) {
  const page = server.serve_kappa_page(k);
  if (!page) throw new Error("publisher missing page " + k);
  await writeFile(path.join(OUT, "k", safe(k)), page);
  bytes += page.length;
}

// Round-trip verification (the loader's flow, headless): fetch the manifest by κ + verify, then
// stream pages by κ from the written files, and confirm the resumed machine is bit-exact.
const get = async (k) => new Uint8Array(await readFile(path.join(OUT, "k", safe(k))));
const gotManifest = await get(manifestKappa);
if (!hs.verify_kappa(gotManifest, manifestKappa)) throw new Error("published manifest fails κ");
const pages = new Map();
for (const k of pageKappas) pages.set(k, await get(k));
const resumed = hs.X64Workspace.resume_kappa_streamed(gotManifest, (k) => pages.get(k) || null);
if (resumed.terminal() !== server.terminal()) throw new Error("published bundle is not bit-exact");

await writeFile(path.join(OUT, "link.txt"), `holo://#k=${manifestKappa}\n`);
console.log(
  "PUBLISHED a shareable κ-link bundle:\n" +
    `  out       : ${path.relative(WEB, OUT)}/k/  (${pageKappas.length + 1} κ-objects, ${(bytes / 1048576).toFixed(1)} MiB)\n` +
    `  manifest  : ${(manifest.length / 1024).toFixed(0)} KiB, addressed by its own κ\n` +
    "  verified  : the bundle round-trips to a BIT-EXACT live machine (manifest + every page by κ, L5)\n" +
    `  LINK      : holo://#k=${manifestKappa}\n` +
    "  → serve link-bundle/ statically and open resume-link.html#k=<κ> to land in a live machine.",
);
