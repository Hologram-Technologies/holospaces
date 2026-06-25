// x64-alpine-boot-test.mjs — witness REAL Alpine x86-64 booting to an interactive shell IN THE BROWSER
// over a streamed OPFS κ-disk, via the holospaces wasm peer (X64Workspace) in headless Chromium.
// M1 of the execution plan: x86 Alpine → a shell you can type into, every sector served from κ.
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
    const w = new Worker("./x64-alpine-boot-worker.mjs", { type: "module" });
    const result = await new Promise((resolve) => {
      w.onmessage = (e) => resolve(e.data);
      w.onerror = (e) => resolve({ ok: false, error: "worker error: " + (e.message || `${e.filename}:${e.lineno}`) });
      w.postMessage("go");
    });
    w.terminate();
    return result;
  });
  if (!r.ok) { failed = true; console.error("X64-ALPINE-BOOT-TEST: worker failed —", r.error); }
  else {
    check(r.mounted, "the x86-64 kernel mounted the Alpine ext4 root over the streamed OPFS κ-disk — in the browser");
    check(r.userspace, "Alpine reached an interactive shell (busybox) — in the browser");
    check(r.alpineRelease, "the live shell printed a real Alpine version (/etc/alpine-release ~ 3.2x.y)");
    check(r.arch, "uname -m == x86_64 (it's really the amd64 userland)");
    console.error("  shell tail:\n" + r.tail);
  }
  console.log(failed ? "X64-ALPINE-BOOT-TEST: FAILED" : "X64-ALPINE-BOOT-TEST: PASS (real Alpine x86-64 booted to a shell in the browser over the streamed OPFS κ-disk)");
} finally { await browser.close(); server.close(); }
process.exit(failed ? 1 : 0);
