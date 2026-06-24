// CC-45 — a provisioned **amd64 (x86-64)** devcontainer BOOTS in the browser via
// the shipped X64Workspace paged-κ-disk path.
//
// The deployed x86-64 analogue of the CC-50 streaming boot: a stock linux/amd64
// devcontainer layer is assembled — sparse, straight into an OPFS file — by the
// SAME streaming serializer DevcontainerProvision.assembleIntoOpfs uses, then BOOTED
// on the x86-64 system core via the shipped `X64Workspace.boot_devcontainer_opfs_
// streamed(kernel, rootfsHandle, diskHandle)` path (the κ-disk paged sector-by-sector
// from OPFS). x86-64 has no device tree, so the κ-disk is discovered from the kernel
// command line (`virtio_mmio.device=…`) the X64Workspace sets — the regression guard
// for "the deployed amd64 boot can't find its disk". Runs inside a real dedicated
// Worker (sync access handles are worker-only — the deployed extension-host context)
// in Chromium, then asserts the guest reaches the deployed userspace marker.
import http from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import path from "node:path";
import { chromium } from "playwright";

const DIR = path.dirname(fileURLToPath(import.meta.url));
const TYPES = { ".html": "text/html", ".js": "text/javascript", ".wasm": "application/wasm", ".css": "text/css", ".mjs": "text/javascript", ".gz": "application/gzip" };

const WORKER_PATH = "/cc45-x64-boot-worker.mjs";
const WORKER_SRC = `
self.addEventListener("error", (e) => { try { self.postMessage({ ok: false, error: "worker error: " + (e.message||"") + " @ " + (e.filename||"") + ":" + (e.lineno||"") }); } catch {} });
self.addEventListener("unhandledrejection", (e) => { try { self.postMessage({ ok: false, error: "unhandledrejection: " + String(e.reason && e.reason.stack ? e.reason.stack : e.reason) }); } catch {} });

const bytes = async (p) => new Uint8Array(await (await fetch(p)).arrayBuffer());
const gunzip = async (b) =>
  new Uint8Array(await new Response(new Response(b).body.pipeThrough(new DecompressionStream("gzip"))).arrayBuffer());

self.onmessage = async () => {
  try {
    const mod = await import("./pkg/holospaces_web.js");
    const { default: init, DevcontainerImage, X64Workspace } = mod;
    await init();

    // The stock linux/amd64 busybox layer + the amd64 Linux kernel — the CC-45
    // fixtures, the same bytes the run-stage witness boots.
    const layer = await bytes("./cc45-x64-layer.tar.gz");
    const kernel = await gunzip(await bytes("./cc45-x64-kernel.gz"));

    const root = await navigator.storage.getDirectory();
    const ROOTFS = "cc45-x64-rootfs.img";
    const PACK = "cc45-x64-disk.pack";
    for (const n of [ROOTFS, PACK]) { try { await root.removeEntry(n); } catch {} }
    const rootfsFile = await root.getFileHandle(ROOTFS, { create: true });
    const rootfsHandle = await rootfsFile.createSyncAccessHandle();

    // Stream the bootable amd64 rootfs sparse into OPFS — a 512 MiB declared disk
    // over a few-MiB busybox layer (the free space is a hole).
    const DISK_BYTES = 512 * 1024 * 1024;
    const img = new DevcontainerImage();
    img.add_layer("application/vnd.oci.image.layer.v1.tar+gzip", layer);
    const imageLen = img.assembleBootableIntoOpfs(rootfsHandle, DISK_BYTES);
    const onDiskLen = rootfsHandle.getSize();

    rootfsHandle.close();
    const rootfsRead = await (await root.getFileHandle(ROOTFS)).createSyncAccessHandle();
    const packHandle = await (await root.getFileHandle(PACK, { create: true })).createSyncAccessHandle();

    // BOOT on the x86-64 core via the shipped paged-κ-disk path.
    const ws = X64Workspace.boot_devcontainer_opfs_streamed(kernel, rootfsRead, packHandle);

    let booted = false, mounted = false;
    for (let i = 0; i < 6000; i++) {
      const halted = ws.run(8_000_000);
      const t = ws.terminal();
      mounted = mounted || t.includes("EXT4-fs") || t.includes("Mounted root");
      booted = booted || t.includes("holospace devcontainer ready");
      if (halted || booted) break;
    }
    const tail = ws.terminal().split("\\n").slice(-12).join("\\n");
    try { rootfsRead.close(); } catch {}
    try { packHandle.close(); } catch {}
    self.postMessage({ ok: true, imageLen, onDiskLen, diskBytes: DISK_BYTES, booted, mounted, tail });
  } catch (e) {
    self.postMessage({ ok: false, error: String(e && e.stack ? e.stack : e) });
  }
};
`;

const server = http.createServer(async (req, res) => {
  const rel = req.url === "/" ? "/index.html" : req.url.split("?")[0];
  if (rel === WORKER_PATH) { res.writeHead(200, { "content-type": "text/javascript" }); res.end(WORKER_SRC); return; }
  try {
    const body = await readFile(path.join(DIR, rel));
    res.writeHead(200, { "content-type": TYPES[path.extname(rel)] || "application/octet-stream" });
    res.end(body);
  } catch { res.writeHead(404).end("not found"); }
});

let failed = false;
const check = (c, m) => (c ? console.log("  ✓", m) : ((failed = true), console.error("CC45-X64-BOOT-TEST: FAIL —", m)));

await new Promise((r) => server.listen(0, "127.0.0.1", r));
const port = server.address().port;
const browser = await chromium.launch();
const page = await (await browser.newContext()).newPage();
page.on("pageerror", (e) => ((failed = true), console.error("CC45-X64-BOOT-TEST: pageerror —", e.message)));
page.on("console", (m) => { if (m.type() === "error") console.error("  [page console]", m.text()); });

try {
  await page.goto(`http://127.0.0.1:${port}/index.html`);
  await page.waitForFunction("window.hs !== undefined", null, { timeout: 30000 });
  const r = await page.evaluate(async (workerPath) => {
    const w = new Worker(workerPath, { type: "module" });
    const result = await new Promise((resolve) => {
      w.onmessage = (e) => resolve(e.data);
      w.onerror = (e) => resolve({ ok: false, error: "worker error: " + (e.message || e.filename + ":" + e.lineno) });
      w.postMessage("go");
    });
    w.terminate();
    return result;
  }, WORKER_PATH);

  if (!r.ok) {
    failed = true;
    console.error("CC45-X64-BOOT-TEST: worker failed —", r.error);
  } else {
    check(r.imageLen >= r.diskBytes && r.onDiskLen === r.imageLen,
      `the bootable amd64 rootfs streamed into OPFS (${r.imageLen} bytes for a ${r.diskBytes}-byte disk, sparse)`);
    check(r.mounted, "a real amd64 Linux mounted the streamed-into-OPFS rootfs over virtio-blk — in the browser");
    check(r.booted, "the amd64 devcontainer BOOTED to userspace on the x86-64 core via the shipped paged-κ-disk path");
    if (!r.booted) console.error("  console tail:\n" + r.tail);
  }
  console.log(failed
    ? "CC45-X64-BOOT-TEST: FAILED"
    : "CC45-X64-BOOT-TEST: PASS (an amd64 devcontainer booted in the browser via the shipped X64Workspace path)");
} finally {
  await browser.close();
  server.close();
}
process.exit(failed ? 1 : 0);
