// x64-stream-tab-test.mjs — Phase 4 witness IN A BROWSER TAB: a fresh tab STREAMS a resume over
// the network, pulling each unique page one-by-κ (verify-on-receipt) from the published manifest,
// and comes up as a live x86-64 machine — the "shareable link" served as static files.
import http from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const TYPES = {
  ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript",
  ".wasm": "application/wasm", ".bin": "application/octet-stream",
};
const server = http.createServer(async (req, res) => {
  try {
    const p = (req.url || "/").split("?")[0];
    const body = await readFile(path.join(WEB, p === "/" ? "/resume-stream.html" : p));
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    res.setHeader("Cross-Origin-Resource-Policy", "cross-origin");
    res.setHeader("Content-Type", TYPES[path.extname(p)] || "application/octet-stream");
    res.end(body);
  } catch { res.statusCode = 404; res.end("nf"); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

const CHROME = process.env.CHROME_PATH || "/usr/bin/google-chrome";
const browser = await chromium.launch({ executablePath: CHROME, args: ["--no-sandbox"] });
const page = await browser.newContext().then((c) => c.newPage());
page.on("console", (m) => { if (m.type() === "error") console.error("  [page]", m.text().slice(0, 200)); });

const tOpen = Date.now();
await page.goto(`http://127.0.0.1:${port}/resume-stream.html`);
let result = null, last = "";
for (let i = 0; i < 240; i++) {
  await new Promise((r) => setTimeout(r, 500));
  const s = await page.evaluate(() => ({ r: window.__result || null, l: window.__last || null }));
  if (s.l) { const j = JSON.stringify(s.l); if (j !== last) { last = j; console.log("[" + ((Date.now() - tOpen) / 1000).toFixed(0) + "s] " + j.slice(0, 160)); } }
  if (s.r) { result = s.r; break; }
}
const wall = ((Date.now() - tOpen) / 1000).toFixed(1);
await browser.close(); server.close();

if (!result || !result.ok) { console.log("FAIL — streamed resume:", JSON.stringify(result)); process.exit(1); }
console.log("\nPASS — x64 κ-snapshot STREAMED resume IN A BROWSER TAB:");
console.log(`  manifest : ${result.manifestKiB.toFixed(0)} KiB`);
console.log(`  streamed : ${result.pages} unique pages = ${result.mib.toFixed(1)} MiB pulled one-by-κ over HTTP (verify-on-receipt) in ${(result.tFetch / 1000).toFixed(1)} s`);
console.log(`  resume   : ${result.tResume.toFixed(0)} ms → live machine, console ${result.consoleBytes} B`);
console.log(`  tab-open→live : ${wall} s — a fresh tab streamed a running x86-64 machine by κ from a link.`);
process.exit(0);
