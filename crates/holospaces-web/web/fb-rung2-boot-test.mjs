// fb-rung2-boot-test.mjs — RUNG 2 witness under Playwright (we own the browser, so a
// multi-minute boot can run to completion). Boots the real graphical aarch64 kernel over
// a κ-disk; the kernel's fbcon renders onto the emulator's simple-framebuffer; we read
// that framebuffer back and save it as a PNG (the literal guest pixels).
import http from "node:http";
import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { chromium } from "playwright";

const ROOT = "C:/Users/pavel/Desktop/HOLOGRAM";
const PAGE = "/_vendor/holowhat/vendor/holospaces/crates/holospaces-web/web/fb-rung2-boot.html";
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript",
  ".wasm": "application/wasm", ".css": "text/css", ".gz": "application/gzip", ".json": "application/json" };

const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : decodeURIComponent(req.url.split("?")[0]);
  try {
    const body = await readFile(path.join(ROOT, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch { res.writeHead(404).end("not found"); }
});

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch({ args: ["--enable-unsafe-webgpu", "--enable-features=Vulkan"] });
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => console.error("  pageerror:", e.message));
page.on("console", (m) => { const t = m.text(); if (/error|panic|fail/i.test(t)) console.error("  console:", t); });

let final = null;
try {
  await page.goto(`http://127.0.0.1:${port}${PAGE}`, { waitUntil: "load", timeout: 60000 });
  const t0 = Date.now();
  while (Date.now() - t0 < 600000) {
    const s = await page.evaluate(() => {
      const r = window.__rung2 || {};
      return { phase: r.phase, iter: r.iter, drm: r.drm, fbcon: r.fbcon, pixels: r.pixels,
               done: r.done, error: r.error,
               serial: (document.getElementById("serial") || {}).textContent };
    });
    const secs = ((Date.now() - t0) / 1000) | 0;
    console.log(`[${secs}s] ${s.phase || "?"}  iter=${s.iter ?? "-"} drm=${s.drm ? "Y" : "."} fbcon=${s.fbcon ? "Y" : "."} px=${s.pixels ?? 0}`);
    if (s.done || s.error) { final = s; break; }
    await new Promise((r) => setTimeout(r, 12000));
  }

  if (!final) { console.error("RUNG2: TIMEOUT (no done/error in 10min)"); }
  else if (final.error) { console.error("RUNG2: ERROR —", final.error); }
  else {
    console.log("\n=== SERIAL (tail) ===\n" + (final.serial || "(none)"));
    const png = await page.evaluate(() => window.__rung2png || null);
    if (png) {
      const b64 = png.split(",")[1];
      await writeFile(path.join(ROOT, "fb-rung2-framebuffer.png"), Buffer.from(b64, "base64"));
      console.log("\nsaved literal guest framebuffer → fb-rung2-framebuffer.png");
    } else console.log("\n(no framebuffer png captured)");
    const pass = final.fbcon && (final.pixels || 0) > 0;
    console.log(`\nRUNG2: ${pass ? "PASS" : "INCOMPLETE"} — drm=${final.drm} fbcon=${final.fbcon} pixels=${final.pixels}`);
  }
} finally {
  await browser.close();
  server.close();
}
