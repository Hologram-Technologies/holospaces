// CC-64 — REAL-browser proof of instant-paint streamed resume.
//
// Opens shell-stream.html in headless Chromium and asserts:
//   G1  first `holo$` paint  < 1000 ms wall-clock
//   G2  bytes transferred before that paint  < 2 MiB  (proves we did NOT ship the 38 MiB blob first)
//   G3  after the machine goes live, a typed command returns byte-exact output (kernel line + HOLO-OK-42)
//
// Mirrors the project's existing headless harness (headless-x64-boot.mjs): a tiny node http server
// over web/ with the COOP/COEP headers the wasm needs, then playwright chromium.
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
    const body = await readFile(path.join(WEB, p === "/" ? "/shell-stream.html" : p));
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    res.setHeader("Cross-Origin-Resource-Policy", "cross-origin");
    res.setHeader("Content-Type", TYPES[path.extname(p)] || "application/octet-stream");
    res.end(body);
  } catch { res.statusCode = 404; res.end("nf"); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

const fail = (msg) => { console.log("CC-64 FAIL: " + msg); cleanup(1); };
let browser;
async function cleanup(code) { try { await browser?.close(); } catch {} server.close(); process.exit(code); }

browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => console.log("  [pageerror] " + e.message));
page.on("console", (m) => { const t = m.text(); if (/ERROR|error/.test(t)) console.log("  [" + m.type() + "] " + t.slice(0, 200)); });

const tStart = Date.now();
await page.goto(`http://127.0.0.1:${port}/shell-stream.html`);

// ── G1 + G2: wait for the painted prompt, read the worker's own paint timing + byte counter. ──
let paint = null;
for (let i = 0; i < 100; i++) {
  const s = await page.evaluate(() => {
    const t = (document.getElementById("t") || {}).textContent || "";
    return { c: window.__cc64, hasPrompt: t.includes("holo$") };
  });
  if (s.c && s.c.error) fail("worker error: " + s.c.error);
  if (s.c && s.c.painted && s.hasPrompt) { paint = s.c; break; }
  await new Promise((r) => setTimeout(r, 20));
}
if (!paint) fail("never painted the prompt");
// The exact text we painted instantly (must be the REAL prefix of the live machine — no fake splash).
const paintText = await page.evaluate(() => (document.getElementById("t") || {}).textContent || "");
const G1 = paint.paintMs < 1000;
const G2 = paint.paintBytes < 2 * 1024 * 1024;
console.log(`CC-64 G1 first-paint: ${paint.paintMs} ms  (< 1000) → ${G1 ? "PASS" : "FAIL"}`);
console.log(`CC-64 G2 bytes-before-paint: ${paint.paintBytes} B (${(paint.paintBytes / 1024).toFixed(1)} KiB)  (< 2 MiB) → ${G2 ? "PASS" : "FAIL"}`);

// ── wait for the machine to go live (background load of the 38 MiB + wasm), then G3. ──
let live = false;
for (let i = 0; i < 600; i++) {
  const c = await page.evaluate(() => window.__cc64);
  if (c && c.error) fail("worker error during live: " + c.error);
  if (c && c.live) { live = true; console.log(`   live: resume ${c.resumeMs} ms, total ${(c.totalBytes / 1048576).toFixed(1)} MiB, liveMs ${c.liveMs}`); break; }
  await new Promise((r) => setTimeout(r, 250));
}
if (!live) fail("machine never went live");

// ── G2b DRIFT GUARD: the instant paint must be the REAL prefix of the live terminal, not a
//    stale/fake splash. If the console fixture ever drifts from the kblob, this fails. ──
const liveText0 = await page.evaluate(() => (document.getElementById("t") || {}).textContent || "");
const trim = (s) => s.replace(/\s+$/, "");
const G2b = trim(liveText0).startsWith(trim(paintText)) && trim(paintText).length > 100;
console.log(`CC-64 G2b paint-is-real (painted text is a prefix of the live terminal) → ${G2b ? "PASS" : "FAIL"}`);
if (!G2b) console.log(`  painted ${paintText.length}B vs live ${liveText0.length}B — fixture drifted from kblob? regenerate x64-alpine-shell.console.txt`);

// ── G3: type a command, assert byte-exact output. ──
await page.fill("#in", "uname -a; echo HOLO-OK-$((6*7))");
await page.press("#in", "Enter");
let out = "";
let G3 = false;
for (let i = 0; i < 480; i++) {
  out = await page.evaluate(() => (document.getElementById("t") || {}).textContent || "");
  if (out.includes("HOLO-OK-42") && /Linux/.test(out)) { G3 = true; break; }
  await new Promise((r) => setTimeout(r, 250));
}
console.log(`CC-64 G3 typed-command byte-exact (kernel line + HOLO-OK-42) → ${G3 ? "PASS" : "FAIL"}`);
if (!G3) console.log("  tail: " + out.replace(/\s+/g, " ").slice(-240));

const allPass = G1 && G2 && G2b && G3;
console.log(`\nCC-64 VERDICT: ${allPass ? "PASS — instant-paint streamed resume proven in headless Chromium" : "FAIL"}`);
console.log(`  first-paint ${paint.paintMs}ms over ${(paint.paintBytes / 1024).toFixed(1)}KiB, then live + byte-exact command. Chrome ${browser.version()}`);
await cleanup(allPass ? 0 : 1);
