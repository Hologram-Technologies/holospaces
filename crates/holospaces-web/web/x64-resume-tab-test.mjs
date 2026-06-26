// x64-resume-tab-test.mjs — Phase 2 witness: resume a deduped κ-blob in a REAL browser TAB
// (Playwright chromium), timed. Serves the wasm + κ-blob fixture with the COOP/COEP headers a
// Worker needs, opens resume-sm.html, and reports wall-clock from tab-open to a live resumed
// console — proving the κ-snapshot resume works in a browser, not just node.
//
// Prereqs: wasm-pack build .. --target web --out-dir web/pkg ; and the κ-blob fixture
// (web/fixtures/x64-resume-snapshot.kblob{,.kappa}) — see make-x64-kblob-fixture.mjs.
import http from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const TYPES = {
  ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript",
  ".wasm": "application/wasm", ".gz": "application/gzip", ".kappa": "text/plain",
  ".kblob": "application/octet-stream", ".bin": "application/octet-stream",
};

const server = http.createServer(async (req, res) => {
  try {
    const p = (req.url || "/").split("?")[0];
    const body = await readFile(path.join(WEB, p === "/" ? "/resume-sm.html" : p));
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    res.setHeader("Cross-Origin-Resource-Policy", "cross-origin");
    res.setHeader("Content-Type", TYPES[path.extname(p)] || "application/octet-stream");
    res.end(body);
  } catch { res.statusCode = 404; res.end("nf"); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

// Playwright ships no browser here — drive the system Chrome instead.
const CHROME = process.env.CHROME_PATH || "/usr/bin/google-chrome";
const browser = await chromium.launch({ executablePath: CHROME, args: ["--no-sandbox"] });
const page = await browser.newContext().then((c) => c.newPage());
page.on("console", (m) => { const t = m.text(); if (!/powerPreference|adapters/.test(t)) console.log("  [" + m.type() + "] " + t.slice(0, 200)); });
page.on("pageerror", (e) => console.log("  [pageerror] " + e.message));

const tOpen = Date.now();
await page.goto(`http://127.0.0.1:${port}/resume-sm.html`);

let result = null, last = "";
for (let i = 0; i < 120; i++) {
  await new Promise((r) => setTimeout(r, 500));
  const s = await page.evaluate(() => ({ r: window.__resumeResult || null, l: window.__last || null }));
  if (s.l) { const j = JSON.stringify(s.l); if (j !== last) { last = j; console.log("[" + ((Date.now() - tOpen) / 1000).toFixed(1) + "s] " + j.slice(0, 200)); } }
  if (s.r) { result = s.r; break; }
}
const wall = ((Date.now() - tOpen) / 1000).toFixed(2);
await browser.close(); server.close();

if (!result || !result.ok) { console.log("FAIL — in-tab resume:", JSON.stringify(result)); process.exit(1); }
console.log("\nPASS — x64 κ-snapshot RESUME witness IN A BROWSER TAB (Playwright chromium):");
console.log(`  κ-blob loaded : ${(result.blobBytes / 1048576).toFixed(1)} MiB (unique pages only)`);
console.log(`  resume_kappa  : ${result.tResume.toFixed(1)} ms  → console preserved (${result.consoleBytes} B), L5-verified`);
console.log(`  ran 16M       : ${result.tRun.toFixed(0)} ms  → machine LIVE (+${result.grew} B)`);
console.log(`  tab-open→live : ${wall} s wall-clock — a running guest in a fresh tab, never booting.`);
process.exit(0);
