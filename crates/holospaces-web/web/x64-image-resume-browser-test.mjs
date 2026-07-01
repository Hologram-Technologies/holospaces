// CC-65 G5 — a real SERVER IMAGE opens instantly in a real browser AND keeps serving.
//
// Opens image-stream.html (the generic any-image resumer) on the warm server-image .holo in headless
// Chromium and asserts:
//   G1  first paint  < 1000 ms        (the boot-log/HOLO-SERVED header, already in the snapshot)
//   G2  bytes before paint  < 2 MiB   (header only — proves streaming, not the 58 MiB blob)
//   G2b paint-is-real                 (painted text is a byte-prefix of the live terminal)
//   G5  KEEPS SERVING IN THE BROWSER  (max HOLO-SERVED-N grows beyond the snapshot's value once live —
//                                      the resumed server + its loopback TCP sockets are alive in the tab)
import http from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const WEB = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".txt": "text/plain", ".kblob": "application/octet-stream", ".css": "text/css" };
const server = http.createServer(async (req, res) => {
  try {
    const p = (req.url || "/").split("?")[0];
    const body = await readFile(path.join(WEB, p === "/" ? "/image-stream.html" : p));
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
async function cleanup(code) { try { await browser?.close(); } catch {} server.close(); process.exit(code); }
const fail = (m) => { console.log("CC-65 G5 FAIL: " + m); cleanup(1); };
const maxServed = (s) => (s.match(/HOLO-SERVED-(\d+)/g) || []).map((x) => +x.split("-")[2]).reduce((a, b) => Math.max(a, b), 0);

browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => console.log("  [pageerror] " + e.message));

const url = `http://127.0.0.1:${port}/image-stream.html?kblob=fixtures/x64-server-image.kblob&header=fixtures/x64-server-image.console.txt`;
await page.goto(url);

// G1 + G2: wait for the painted header.
let paint = null;
for (let i = 0; i < 100; i++) {
  const s = await page.evaluate(() => ({ c: window.__cc64, t: (document.getElementById("t") || {}).textContent || "" }));
  if (s.c && s.c.error) fail("worker error: " + s.c.error);
  if (s.c && s.c.painted && s.t.includes("HOLO-SERVED")) { paint = s.c; break; }
  await new Promise((r) => setTimeout(r, 20));
}
if (!paint) fail("never painted the server header");
const paintText = await page.evaluate(() => (document.getElementById("t") || {}).textContent || "");
const servedAtPaint = maxServed(paintText);
const G1 = paint.paintMs < 1000;
const G2 = paint.paintBytes < 2 * 1024 * 1024;
console.log(`CC-65 G5/G1 first-paint: ${paint.paintMs} ms  (< 1000) → ${G1 ? "PASS" : "FAIL"}`);
console.log(`CC-65 G5/G2 bytes-before-paint: ${(paint.paintBytes / 1024).toFixed(1)} KiB  (< 2 MiB) → ${G2 ? "PASS" : "FAIL"}  [served@paint=${servedAtPaint}]`);

// Wait for live.
let live = false;
for (let i = 0; i < 600; i++) {
  const c = await page.evaluate(() => window.__cc64);
  if (c && c.error) fail("worker error during live: " + c.error);
  if (c && c.live) { live = true; console.log(`   live: resume ${c.resumeMs} ms, ${(c.totalBytes / 1048576).toFixed(1)} MiB total`); break; }
  await new Promise((r) => setTimeout(r, 250));
}
if (!live) fail("server image never went live");

// G2b drift guard.
const liveText0 = await page.evaluate(() => (document.getElementById("t") || {}).textContent || "");
const trim = (s) => s.replace(/\s+$/, "");
const G2b = trim(liveText0).startsWith(trim(paintText)) && trim(paintText).length > 100;
console.log(`CC-65 G5/G2b paint-is-real → ${G2b ? "PASS" : "FAIL"}`);

// G5: the server KEEPS SERVING in the browser — HOLO-SERVED grows beyond the snapshot value.
let after = servedAtPaint;
for (let i = 0; i < 480; i++) {
  after = maxServed(await page.evaluate(() => (document.getElementById("t") || {}).textContent || ""));
  if (after > servedAtPaint) break;
  await new Promise((r) => setTimeout(r, 250));
}
const G5 = after > servedAtPaint;
console.log(`CC-65 G5 keeps-serving-in-browser: HOLO-SERVED ${servedAtPaint} → ${after} → ${G5 ? "PASS" : "FAIL"}`);

const allPass = G1 && G2 && G2b && G5;
console.log(`\nCC-65 G5 VERDICT: ${allPass ? "PASS — a server image opens instantly in a real browser and keeps serving" : "FAIL"}`);
console.log(`  first-paint ${paint.paintMs}ms / ${(paint.paintBytes / 1024).toFixed(1)}KiB, then live server served ${servedAtPaint}→${after} in-tab. Chrome ${browser.version()}`);
await cleanup(allPass ? 0 : 1);
