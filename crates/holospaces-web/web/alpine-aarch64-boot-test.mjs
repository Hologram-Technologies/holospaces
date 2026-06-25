// alpine-aarch64-boot-test.mjs — witness REAL Alpine booting to an interactive shell over a
// per-sector OPFS κ-disk, in headless Chromium, via the holospaces wasm peer.
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
    const body = await readFile(path.join(DIR, p === "/" ? "/index.html" : p));
    res.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    res.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
    res.setHeader("Cross-Origin-Resource-Policy", "cross-origin");
    res.setHeader("Content-Type", TYPES[path.extname(path.join(DIR, p))] || "application/octet-stream");
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
    const w = new Worker("./alpine-aarch64-boot-worker.mjs", { type: "module" });
    const result = await new Promise((resolve) => {
      w.onmessage = (e) => resolve(e.data);
      w.onerror = (e) => resolve({ ok: false, error: "worker error: " + (e.message || `${e.filename}:${e.lineno}`) });
      w.postMessage("go");
    });
    w.terminate();
    return result;
  });
  if (!r.ok) { failed = true; console.error("ALPINE-AARCH64: worker failed —", r.error); }
  else {
    check(r.imageLen > 0, `the Alpine riscv64 minirootfs assembled into a bootable ext4 in the browser (${r.imageLen} bytes, sparse over ${r.diskBytes})`);
    check(r.mounted, "a real Linux mounted the Alpine rootfs over the emulator's virtio-blk, paged from OPFS — in the browser");
    check(r.userspace, "Alpine userland reached an interactive shell over the per-sector κ-disk (`holospace:/workspace#`) — in the browser");
    check(r.alpineRelease, "the LIVE root is really Alpine — `cat /etc/alpine-release` returned 3.2x");
    console.log(`  ${r.apk ? "✓" : "·"} apk-tools present in console (best-effort)`);
    if (!r.userspace || !r.alpineRelease) console.error("  console tail:\n" + r.tail);
  }
  console.log(failed ? "ALPINE-AARCH64-BOOT-TEST: FAILED" : "ALPINE-AARCH64-BOOT-TEST: PASS (real interactive Alpine booted in the browser over the per-sector OPFS κ-disk)");
} finally { await browser.close(); server.close(); }
process.exit(failed ? 1 : 0);
