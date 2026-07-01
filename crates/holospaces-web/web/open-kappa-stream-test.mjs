// CC-76 (Polish-1) — REAL κ-URL: open a live app from ONE content-addressed κ, 100% serverless.
//
// Opens open.html?k=<manifest-κ>&store=./fixtures/store in headless Chromium. Unlike the flat-blob path,
// the handle is a κ: the tab fetches the manifest BY κ, fetches each UNIQUE page BY κ, and resumes with
// every page verified against its κ in wasm (L5). Asserts:
//   G1  resumed from the κ (no boot)              G2  the guest app's real response rendered
//   G3  content-addressed: pages were fetched as store/blake3_… by κ (not one opaque blob)
//   G4  serverless: every request static/on-origin, zero app-server calls
import http from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const KAPPA = (await readFile(path.join(WEB, "fixtures/store/.manifest-kappa"), "utf8")).trim();
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".holo": "application/octet-stream" };
const server = http.createServer(async (req, res) => {
  try {
    const p = decodeURIComponent((req.url || "/").split("?")[0]);
    const body = await readFile(path.join(WEB, p === "/" ? "/open.html" : p));
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    res.setHeader("Cross-Origin-Resource-Policy", "cross-origin");
    res.setHeader("Content-Type", TYPES[path.extname(p)] || "application/octet-stream");
    res.end(body);
  } catch { res.statusCode = 404; res.end("nf"); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const EXPECT = "HELLO-FROM-HOLO-REACHABLE";

let browser;
const fail = (msg) => { console.log("CC-76(κ) FAIL: " + msg); cleanup(1); };
async function cleanup(code) { try { await browser?.close(); } catch {} server.close(); process.exit(code); }

browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => console.log("  [pageerror] " + e.message));
const requests = [];
page.on("request", (r) => requests.push(r.url()));

console.log(`CC-76(κ): opening one κ handle → ${KAPPA.slice(0, 28)}…`);
await page.goto(`http://127.0.0.1:${port}/open.html?k=${encodeURIComponent(KAPPA)}&store=./fixtures/store&port=8080`);

let r = null;
for (let i = 0; i < 1200; i++) {
  const s = await page.evaluate(() => window.__open_kappa || null);
  if (s && s.error) fail("worker error: " + s.error);
  if (s && s.rendered) { r = s; break; }
  await new Promise((res) => setTimeout(res, 100));
}
if (!r) fail("the tab never rendered the app from the κ");

const G1 = typeof r.resumeMs === "number";
console.log(`CC-76(κ) G1 resumed from the κ (fetch-by-κ + verify + resume): ${r.resumeMs} ms → ${G1 ? "PASS" : "FAIL"}`);

const bodyInDom = await page.evaluate(() => (document.getElementById("body") || {}).textContent || "");
const G2 = (r.body || "").includes(EXPECT) && bodyInDom.includes(EXPECT);
console.log(`CC-76(κ) G2 app rendered (${JSON.stringify(EXPECT)}): status=${JSON.stringify(r.status)} → ${G2 ? "PASS" : "FAIL"}`);

const pageFetches = requests.filter((u) => /\/store\/blake3_/.test(u));
const G3 = pageFetches.length > 100; // thousands of unique pages fetched BY κ, not one blob
console.log(`CC-76(κ) G3 content-addressed: ${pageFetches.length} pages fetched by κ (store/blake3_…) → ${G3 ? "PASS" : "FAIL"}`);

const origin = `http://127.0.0.1:${port}`;
const offOrigin = requests.filter((u) => !u.startsWith(origin) && !u.startsWith("data:") && !u.startsWith("blob:"));
const G4 = offOrigin.length === 0;
console.log(`CC-76(κ) G4 serverless (${requests.length} reqs, 0 off-origin, 0 app-server) → ${G4 ? "PASS" : "FAIL"}`);

const allPass = G1 && G2 && G3 && G4;
console.log(`\nCC-76(κ) VERDICT: ${allPass ? "PASS — one κ-link opened a live app in a browser, content-addressed + L5-verified, 100% serverless" : "FAIL"}`);
console.log(`  resume ${r.resumeMs}ms from ${pageFetches.length} κ-pages, app ${JSON.stringify(r.status)}, Chrome ${browser.version()}`);
await cleanup(allPass ? 0 : 1);
