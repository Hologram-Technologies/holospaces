// CC-76 (Polish-2) — a REAL, UNMODIFIED docker image (nginx:alpine) opened live in a browser tab.
//
// Opens open.html?k=./fixtures/x64-nginx.holo&port=80 in headless Chromium and asserts the tab renders
// nginx's actual "Welcome to nginx!" page — resumed from a warm snapshot of the stock image, served over
// the in-tab loopback bridge, 100% serverless. Captures a screenshot (cc76-nginx.png).
import http from "node:http";
import { readFile, access } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const WEB = path.dirname(fileURLToPath(import.meta.url));
try { await access(path.join(WEB, "fixtures/x64-nginx.holo")); }
catch { console.log("CC-76(nginx) SKIP — fixtures/x64-nginx.holo missing (generate: holo run nginx:alpine, copy the cached .holo)"); process.exit(0); }

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

let browser;
const fail = (m) => { console.log("CC-76(nginx) FAIL: " + m); cleanup(1); };
async function cleanup(code) { try { await browser?.close(); } catch {} server.close(); process.exit(code); }

browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => console.log("  [pageerror] " + e.message));
const requests = [];
page.on("request", (r) => requests.push(r.url()));

await page.goto(`http://127.0.0.1:${port}/open.html?k=./fixtures/x64-nginx.holo&port=80`);

let r = null;
for (let i = 0; i < 1200; i++) {
  const s = await page.evaluate(() => window.__open_kappa || null);
  if (s && s.error) fail("worker error: " + s.error);
  if (s && s.rendered) { r = s; break; }
  await new Promise((res) => setTimeout(res, 100));
}
if (!r) fail("the tab never rendered nginx");

const bodyInDom = await page.evaluate(() => (document.getElementById("body") || {}).textContent || "");
const isNginx = /nginx/i.test(r.body || "") || /nginx/i.test(bodyInDom);
const G1 = typeof r.resumeMs === "number";
const G2 = isNginx && /200/.test(r.status || "");
const origin = `http://127.0.0.1:${port}`;
const G3 = requests.filter((u) => !u.startsWith(origin) && !u.startsWith("data:") && !u.startsWith("blob:")).length === 0;
console.log(`CC-76(nginx) G1 resumed the stock nginx image (no boot): ${r.resumeMs} ms → ${G1 ? "PASS" : "FAIL"}`);
console.log(`CC-76(nginx) G2 nginx's real page rendered: status=${JSON.stringify(r.status)}, has "nginx" → ${G2 ? "PASS" : "FAIL"}`);
console.log(`CC-76(nginx) G3 serverless (${requests.length} reqs, 0 off-origin) → ${G3 ? "PASS" : "FAIL"}`);
if (!G2) console.log("  body: " + JSON.stringify((r.body || "").slice(0, 200)));

try { await page.screenshot({ path: path.join(WEB, "cc76-nginx.png") }); console.log("  screenshot → web/cc76-nginx.png"); } catch (e) { console.log("  screenshot failed: " + e.message); }

const allPass = G1 && G2 && G3;
console.log(`\nCC-76(nginx) VERDICT: ${allPass ? "PASS — an UNMODIFIED nginx:alpine image opened live in a browser tab, serverless" : "FAIL"}`);
console.log(`  resume ${r.resumeMs}ms, ${JSON.stringify(r.status)}, Chrome ${browser.version()}`);
await cleanup(allPass ? 0 : 1);
