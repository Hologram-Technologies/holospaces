// fb-rung3-boot-test.mjs — RUNG 3 witness under Playwright. Boots the graphical kernel
// with a custom init that launches cage (Wayland) on the simple-framebuffer; captures the
// literal guest framebuffer as a PNG.
import http from "node:http";
import { readFile, writeFile } from "node:fs/promises";
import path from "node:path";
import { chromium } from "playwright";

const ROOT = "C:/Users/pavel/Desktop/HOLOGRAM";
const PAGE = "/_vendor/holowhat/vendor/holospaces/crates/holospaces-web/web/fb-rung3-boot.html";
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

let final = null;
try {
  await page.goto(`http://127.0.0.1:${port}${PAGE}`, { waitUntil: "load", timeout: 60000 });
  const t0 = Date.now();
  while (Date.now() - t0 < 780000) {   // up to 13 min (bigger image + cage startup)
    const s = await page.evaluate(() => {
      const r = window.__rung3 || {};
      return { phase: r.phase, iter: r.iter, dri: r.dri, wlr: r.wlr, cage: r.cage, exited: r.exited,
               pixels: r.pixels, done: r.done, error: r.error,
               serial: (document.getElementById("serial") || {}).textContent };
    });
    const secs = ((Date.now() - t0) / 1000) | 0;
    console.log(`[${secs}s] ${s.phase || "?"}  iter=${s.iter ?? "-"} dri=${s.dri?"Y":"."} wlr=${s.wlr?"Y":"."} session=${s.cage?"Y":"."} px=${s.pixels ?? 0}`);
    if (s.done || s.error) { final = s; break; }
    await new Promise((r) => setTimeout(r, 12000));
  }

  if (!final) console.error("RUNG3: TIMEOUT");
  else if (final.error) console.error("RUNG3: ERROR —", final.error);
  else {
    console.log("\n=== SERIAL (tail) ===\n" + (final.serial || "(none)"));
    const png = await page.evaluate(() => window.__rung3png || null);
    if (png) {
      await writeFile(path.join(ROOT, "fb-rung3-framebuffer.png"), Buffer.from(png.split(",")[1], "base64"));
      console.log("\nsaved literal guest framebuffer → fb-rung3-framebuffer.png");
    }
    const pass = final.cage && final.wlr && (final.pixels || 0) > 0;
    console.log(`\nRUNG3: ${pass ? "PASS" : "INCOMPLETE"} — dri=${final.dri} wlr=${final.wlr} session=${final.cage} pixels=${final.pixels}`);
  }
} finally {
  await browser.close();
  server.close();
}
