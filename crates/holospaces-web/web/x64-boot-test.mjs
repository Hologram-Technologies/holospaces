// x64-boot-test.mjs — witness real x86-64 Linux booting IN THE BROWSER over a streamed OPFS κ-disk,
// via the holospaces wasm peer (X64Workspace) in headless Chromium.
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".wasm": "application/wasm", ".css": "text/css", ".gz": "application/gzip" };
const server = http.createServer(async (req, res) => {
  try {
    const p = (req.url || "/").split("?")[0];
    const file = path.join(DIR, p === "/" ? "/index.html" : p);
    const body = await readFile(file);
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    res.setHeader("Cross-Origin-Resource-Policy", "cross-origin");
    res.setHeader("Content-Type", TYPES[path.extname(file)] || "application/octet-stream");
    res.end(body);
  } catch { res.statusCode = 404; res.end("not found"); }
});
await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;

const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("console", (m) => { if (m.type() === "error") console.error("  [page]", m.text()); });
let failed = false;
const check = (c, m) => { console.log((c ? "  ✓ " : "  ✗ ") + m); if (!c) failed = true; };

try {
  await page.goto(`http://127.0.0.1:${port}/index.html`);
  await page.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });
  const r = await page.evaluate(async () => {
    const w = new Worker("./x64-boot-worker.mjs", { type: "module" });
    const result = await new Promise((resolve) => {
      w.onmessage = (e) => resolve(e.data);
      w.onerror = (e) => resolve({ ok: false, error: "worker error: " + (e.message || `${e.filename}:${e.lineno}`) });
      w.postMessage("go");
    });
    w.terminate();
    return result;
  });
  if (!r.ok) { failed = true; console.error("X64-BOOT-TEST: worker failed —", r.error); }
  else {
    check(r.handed, "the x86-64 core handed control to PID 1 with the streamed OPFS κ-disk attached — in the browser");
    check(r.userspace, "real amd64 Linux booted to userspace over the OPFS κ-disk — in the browser (HOLOSPACES-LINUX-USERSPACE-OK)");
    if (!r.userspace) console.error("  console tail:\n" + r.tail);
    if (r.excTrace) console.error("\n  === exception trace (last, most recent at bottom) ===\n" + r.excTrace);
  }
  console.log(failed ? "X64-BOOT-TEST: FAILED" : "X64-BOOT-TEST: PASS (real x86-64 Linux booted in the browser over the streamed OPFS κ-disk)");
} finally { await browser.close(); server.close(); }
process.exit(failed ? 1 : 0);
