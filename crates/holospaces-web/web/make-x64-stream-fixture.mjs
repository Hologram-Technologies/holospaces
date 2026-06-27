// make-x64-stream-fixture.mjs — publish a κ-snapshot for streaming over HTTP: resume the κ-blob
// fixture into a live machine, seal it, and write the manifest + each UNIQUE page as its own file
// keyed by κ (web/fixtures/stream/{manifest.bin,pages/<κ>}). The in-tab streaming witness fetches
// these by κ — the "shareable link" served as static files.
import { readFile, writeFile, mkdir } from "node:fs/promises";
import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const hs = require(path.join(WEB, "pkg-node", "holospaces_web.js"));
const safe = (k) => k.replace(/[:/]/g, "_"); // κ "blake3:<hex>" → a filesystem/URL-safe name

const blob = new Uint8Array(await readFile(path.join(WEB, "fixtures", "x64-resume-snapshot.kblob")));
const server = hs.X64Workspace.resume_kappa(blob);
const manifest = server.suspend_kappa_sealed();

const dir = path.join(WEB, "fixtures", "stream");
const pdir = path.join(dir, "pages");
await mkdir(pdir, { recursive: true });
await writeFile(path.join(dir, "manifest.bin"), manifest);

const ks = hs.kappa_manifest_pages(manifest);
let bytes = 0;
for (const k of ks) {
  const p = server.serve_kappa_page(k);
  await writeFile(path.join(pdir, safe(k)), p);
  bytes += p.length;
}
console.log(`published: manifest ${(manifest.length / 1024).toFixed(0)} KiB + ${ks.length} unique pages = ${(bytes / 1048576).toFixed(1)} MiB → ${dir}`);
