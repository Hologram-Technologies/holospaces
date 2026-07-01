// T0.1 — run the SAME 64MB x86-64 boot (boot-sm.html → x64sm-worker.mjs) in headless Chrome.
// Boots clean here but crashes in CEF ⇒ CEF-149-specific. Crashes here too ⇒ emulator wasm bug.
import http from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
const WEB = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".gz": "application/gzip", ".css": "text/css", ".dtb": "application/octet-stream" };
const server = http.createServer(async (req, res) => {
  try {
    const p = (req.url || "/").split("?")[0];
    const body = await readFile(path.join(WEB, p === "/" ? "/boot-sm.html" : p));
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    res.setHeader("Cross-Origin-Resource-Policy", "cross-origin");
    res.setHeader("Content-Type", TYPES[path.extname(p)] || "application/octet-stream");
    res.end(body);
  } catch { res.statusCode = 404; res.end("nf"); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const ctx = await browser.newContext();
const page = await ctx.newPage();
let crashed = false;
page.on("crash", () => { crashed = true; console.log("!! PAGE CRASHED (headless Chrome)"); });
page.on("console", (m) => { const t = m.text(); if (!/powerPreference|No available adapters/.test(t)) console.log("  [" + m.type() + "] " + t.slice(0, 160)); });
page.on("pageerror", (e) => console.log("  [pageerror] " + e.message));
await page.goto(`http://127.0.0.1:${port}/boot-sm.html`);
const t0 = Date.now();
let last = "";
for (let i = 0; i < 90 && !crashed; i++) {
  await new Promise((r) => setTimeout(r, 2000));
  let s;
  try { s = await page.evaluate(() => ({ r: window.__bootResult || null, t: (document.getElementById("t") || {}).textContent || "", last: window.__last || null })); }
  catch (e) { console.log("  (eval failed: " + e.message + ")"); break; }
  if (s.t && s.t !== last) { last = s.t; console.log("[" + Math.round((Date.now() - t0) / 1000) + "s] " + s.t.replace(/\s+/g, " ").slice(0, 200)); }
  if (s.r) { console.log("=== RESULT === " + JSON.stringify(s.r).slice(0, 600)); break; }
}
console.log(crashed ? "VERDICT: CRASHED in headless Chrome too ⇒ emulator wasm bug" : "VERDICT: ran in headless Chrome (no crash) — see result above");
console.log("Chrome:", browser.version());
await browser.close(); server.close(); process.exit(0);
