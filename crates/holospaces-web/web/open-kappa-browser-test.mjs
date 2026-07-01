// CC-76 — REAL-browser proof of `open(κ)`: one link → a live app in a tab, 100% serverless.
//
// Opens open.html?k=<warm .holo> in headless Chromium and asserts:
//   G1  the tab RESUMES the warm .holo (no boot) and the resume is fast
//   G2  the guest app's REAL response is rendered in the page (window.__open_kappa.body)
//   G3  SERVERLESS: every network request is a static asset from the test server — zero app-server calls
//       (the app ran entirely inside the tab, reached over the in-tab loopback bridge)
//
// Mirrors CC-64's harness: a tiny node http server over web/ with the COOP/COEP headers the wasm needs.
import http from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".holo": "application/octet-stream", ".kblob": "application/octet-stream", ".txt": "text/plain", ".css": "text/css" };
const server = http.createServer(async (req, res) => {
  try {
    const p = (req.url || "/").split("?")[0];
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
const fail = (msg) => { console.log("CC-76 FAIL: " + msg); cleanup(1); };
async function cleanup(code) { try { await browser?.close(); } catch {} server.close(); process.exit(code); }

browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => console.log("  [pageerror] " + e.message));
page.on("console", (m) => { const t = m.text(); if (/error/i.test(t)) console.log("  [" + m.type() + "] " + t.slice(0, 200)); });

// Record every network request so we can prove the app was served entirely in-tab (no app server).
const requests = [];
page.on("request", (r) => requests.push(r.url()));

await page.goto(`http://127.0.0.1:${port}/open.html?k=./fixtures/x64-server-loopback.holo&port=8080`);

// Wait for the worker to resume + dial + render (or error).
let r = null;
for (let i = 0; i < 800; i++) {
  const s = await page.evaluate(() => window.__open_kappa || null);
  if (s && s.error) fail("worker error: " + s.error);
  if (s && s.rendered) { r = s; break; }
  await new Promise((res) => setTimeout(res, 100));
}
if (!r) fail("the tab never rendered the app");

// G1 — resumed (not booted), reasonably fast.
const G1 = typeof r.resumeMs === "number" && r.resumeMs < 15000;
console.log(`CC-76 G1 resumed the warm .holo (no boot): ${r.resumeMs} ms → ${G1 ? "PASS" : "FAIL"}`);

// G2 — the guest app's REAL response is on the page.
const bodyInDom = await page.evaluate(() => (document.getElementById("body") || {}).textContent || "");
const G2 = (r.body || "").includes(EXPECT) && bodyInDom.includes(EXPECT);
console.log(`CC-76 G2 app response rendered in the tab (contains ${JSON.stringify(EXPECT)}): status=${JSON.stringify(r.status)} → ${G2 ? "PASS" : "FAIL"}`);
if (!G2) console.log("  body: " + JSON.stringify((r.body || "").slice(0, 160)));

// G3 — serverless: every request went to the static test origin; only static asset types.
const origin = `http://127.0.0.1:${port}`;
const offOrigin = requests.filter((u) => !u.startsWith(origin) && !u.startsWith("data:") && !u.startsWith("blob:"));
const dynamic = requests.filter((u) => /\/(api|cgi|proxy)\b/i.test(u));
const G3 = offOrigin.length === 0 && dynamic.length === 0;
console.log(`CC-76 G3 serverless (all ${requests.length} requests static, 0 off-origin, 0 app-server) → ${G3 ? "PASS" : "FAIL"}`);
if (!G3) console.log("  off-origin/dynamic: " + JSON.stringify(offOrigin.concat(dynamic).slice(0, 8)));

// Capture the rendered page — a real web page served by a κ-resumed server, inside the tab.
try { await page.screenshot({ path: path.join(WEB, "cc76-open-kappa.png"), fullPage: false }); console.log("  screenshot → web/cc76-open-kappa.png"); } catch (e) { console.log("  screenshot failed: " + e.message); }

const allPass = G1 && G2 && G3;
console.log(`\nCC-76 VERDICT: ${allPass ? "PASS — open(κ): a warm image resumed live in a browser tab and served its app, 100% serverless" : "FAIL"}`);
console.log(`  resume ${r.resumeMs}ms, app status ${JSON.stringify(r.status)}, Chrome ${browser.version()}`);
await cleanup(allPass ? 0 : 1);
